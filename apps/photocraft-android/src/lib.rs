#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![deny(unsafe_code)]

#[cfg(any(target_os = "android", test))]
const PICKER_LIMIT: usize = 512 * 1024 * 1024;

/// Decode the private picker bridge payload: a big-endian name length, UTF-8 filename, then file
/// contents. The Java bridge writes this only after the system document provider returns bytes.
#[cfg(any(target_os = "android", test))]
fn parse_picker_payload(mut bytes: Vec<u8>) -> Result<(String, Vec<u8>), String> {
    if bytes.len() > PICKER_LIMIT.saturating_add(4096).saturating_add(4) {
        return Err("selected file exceeds 512 MiB".into());
    }
    let length_bytes: [u8; 4] = bytes.get(..4).ok_or("picker result is incomplete")?.try_into().map_err(|_| "picker result is incomplete")?;
    let name_len = usize::try_from(u32::from_be_bytes(length_bytes)).map_err(|_| "selected file name is too long")?;
    if name_len > 4096 {
        return Err("selected file name is too long".into());
    }
    let name_end = 4usize.checked_add(name_len).ok_or("picker result is invalid")?;
    let name = std::str::from_utf8(bytes.get(4..name_end).ok_or("picker result is incomplete")?).map_err(|_| "selected file name is not UTF-8")?.to_string();
    let contents_len = bytes.len().checked_sub(name_end).ok_or("picker result is incomplete")?;
    bytes.copy_within(name_end.., 0);
    bytes.truncate(contents_len);
    Ok((name, bytes))
}

#[cfg(test)]
mod tests {
    use super::parse_picker_payload;

    #[test]
    fn reads_name_and_image_bytes_from_picker_payload() {
        let mut data = Vec::from(8u32.to_be_bytes());
        data.extend_from_slice(b"wave.png");
        data.extend_from_slice(b"PNG");
        assert_eq!(parse_picker_payload(data).unwrap(), ("wave.png".into(), b"PNG".to_vec()));
    }

    #[test]
    fn rejects_truncated_picker_payloads() {
        assert!(parse_picker_payload(vec![0, 0, 0, 4, b'a']).is_err());
        assert!(parse_picker_payload(vec![0, 0, 0]).is_err());
    }
}

#[cfg(target_os = "android")]
pub mod android {
    use super::parse_picker_payload;
    use android_activity::input::{InputEvent, KeyAction, Keycode, MotionAction};
    use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};
    use egui_wgpu::wgpu;
    use egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor};
    use photocraft_engine::Session;
    use photocraft_ui_egui::theme::ThemeKind;
    use photocraft_ui_egui::{Inbox, PhotocraftApp, Services};
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const OPEN_REQUEST: &str = "photocraft-open-request";
    const OPEN_RESULT: &str = "photocraft-open-result";
    const OPEN_READY: &str = "photocraft-open-ready";

    struct AndroidRenderer {
        surface: wgpu::Surface<'static>,
        surface_config: wgpu::SurfaceConfiguration,
        render_state: egui_wgpu::RenderState,
        pixels_per_point: f32,
    }

    #[derive(Debug)]
    struct AndroidSurfaceWindow(android_activity::ndk::native_window::NativeWindow);

    impl raw_window_handle::HasWindowHandle for AndroidSurfaceWindow {
        fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
            raw_window_handle::HasWindowHandle::window_handle(&self.0)
        }
    }

    impl raw_window_handle::HasDisplayHandle for AndroidSurfaceWindow {
        fn display_handle(&self) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
            Ok(raw_window_handle::DisplayHandle::android())
        }
    }

    pub fn android_main(android: AndroidApp) {
        android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info));
        let Some(data_dir) = android.internal_data_path() else {
            log::error!("PhotoCraft could not access its private app storage");
            return;
        };
        let inbox: Inbox = Arc::default();
        let mut app = PhotocraftApp::new(Session::new(), services(&data_dir, inbox.clone()));
        let context = egui::Context::default();
        PhotocraftApp::setup_context(&context, ThemeKind::Pro);
        app.set_theme(&context, ThemeKind::Pro);

        let start = Instant::now();
        let mut renderer: Option<AndroidRenderer> = None;
        let mut retained_render_state: Option<egui_wgpu::RenderState> = None;
        let mut pending_events = Vec::new();
        let mut focused = false;
        let mut alive = true;
        while alive {
            android.poll_events(Some(Duration::from_millis(16)), |event| match event {
                PollEvent::Main(MainEvent::InitWindow { .. }) => {
                    log::info!("PhotoCraft Android window initialized");
                    renderer = android.native_window().and_then(|window| match initialize_renderer(window, &android, retained_render_state.take()) {
                        Ok(state) => {
                            app.set_wgpu(state.render_state.clone());
                            Some(state)
                        }
                        Err(error) => {
                            log::error!("PhotoCraft could not initialize the Android renderer: {error}");
                            None
                        }
                    });
                }
                PollEvent::Main(MainEvent::TerminateWindow { .. }) => {
                    log::info!("PhotoCraft Android window terminated");
                    app.clear_wgpu();
                    retained_render_state = renderer.take().map(|state| state.render_state);
                    focused = false;
                }
                // Opening the document picker pauses NativeActivity without necessarily
                // destroying its window. Keep the renderer until TerminateWindow so Resume can
                // continue drawing immediately and preserve the active document context.
                PollEvent::Main(MainEvent::Pause) => {
                    log::info!("PhotoCraft Android activity paused");
                    focused = false;
                }
                PollEvent::Main(MainEvent::WindowResized { .. }) => {
                    if let (Some(window), Some(state)) = (android.native_window(), renderer.as_mut()) {
                        resize_renderer(state, &window);
                    }
                }
                PollEvent::Main(MainEvent::GainedFocus) => focused = true,
                PollEvent::Main(MainEvent::LostFocus) => focused = false,
                PollEvent::Main(MainEvent::Resume { .. }) => {
                    log::info!("PhotoCraft Android activity resumed");
                    focused = true;
                    receive_picker_result(&data_dir, &inbox, &context);
                }
                PollEvent::Main(MainEvent::Destroy) => {
                    log::info!("PhotoCraft Android activity destroyed");
                    alive = false;
                }
                PollEvent::Main(MainEvent::LowMemory) => {
                    log::warn!("Android reported low memory");
                }
                PollEvent::Main(MainEvent::InputAvailable) => {
                    if let Ok(mut events) = android.input_events_iter() {
                        loop {
                            let has_more = events.next(|event| match event {
                                InputEvent::MotionEvent(motion) => {
                                    append_motion_events(motion, state_scale(&renderer), &mut pending_events);
                                    InputStatus::Handled
                                }
                                InputEvent::KeyEvent(android_key) => {
                                    let egui_key = match android_key.key_code() {
                                        Keycode::Back => Some(egui::Key::Escape),
                                        Keycode::Enter => Some(egui::Key::Enter),
                                        Keycode::Del => Some(egui::Key::Backspace),
                                        Keycode::Tab => Some(egui::Key::Tab),
                                        Keycode::DpadUp => Some(egui::Key::ArrowUp),
                                        Keycode::DpadDown => Some(egui::Key::ArrowDown),
                                        Keycode::DpadLeft => Some(egui::Key::ArrowLeft),
                                        Keycode::DpadRight => Some(egui::Key::ArrowRight),
                                        _ => None,
                                    };
                                    if let Some(key) = egui_key {
                                        pending_events.push(egui::Event::Key {
                                            key,
                                            physical_key: Some(key),
                                            pressed: android_key.action() == KeyAction::Down,
                                            repeat: android_key.repeat_count() > 0,
                                            modifiers: egui::Modifiers::default(),
                                        });
                                        InputStatus::Handled
                                    } else {
                                        InputStatus::Unhandled
                                    }
                                }
                                _ => InputStatus::Unhandled,
                            });
                            if !has_more {
                                break;
                            }
                        }
                    }
                }
                PollEvent::Main(MainEvent::RedrawNeeded { .. }) => {
                    if let Some(state) = renderer.as_mut() {
                        render_frame(state, &context, &mut app, start.elapsed().as_secs_f64(), std::mem::take(&mut pending_events));
                    }
                }
                _ => {}
            });

            receive_picker_result(&data_dir, &inbox, &context);
            if focused && let Some(state) = renderer.as_mut() {
                let events = std::mem::take(&mut pending_events);
                if !render_frame(state, &context, &mut app, start.elapsed().as_secs_f64(), events) {
                    log::warn!("PhotoCraft skipped an Android frame after a surface error");
                }
            }
        }
        app.clear_wgpu();
    }

    fn state_scale(renderer: &Option<AndroidRenderer>) -> f32 {
        renderer.as_ref().map_or(1.0, |state| state.pixels_per_point)
    }

    fn append_motion_events(motion: &android_activity::input::MotionEvent<'_>, scale: f32, events: &mut Vec<egui::Event>) {
        use egui::{Event, Modifiers, PointerButton, Pos2, Vec2};
        if motion.pointer_count() == 0 {
            return;
        }
        let pointer = motion.pointer_at_index(0);
        let position = Pos2::new(pointer.x() / scale.max(0.01), pointer.y() / scale.max(0.01));
        match motion.action() {
            MotionAction::Down | MotionAction::PointerDown => {
                events.push(Event::PointerMoved(position));
                events.push(Event::PointerButton { pos: position, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::default() });
            }
            MotionAction::Up | MotionAction::PointerUp | MotionAction::Cancel => {
                events.push(Event::PointerMoved(position));
                events.push(Event::PointerButton { pos: position, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::default() });
            }
            MotionAction::Move | MotionAction::HoverMove => events.push(Event::PointerMoved(position)),
            MotionAction::Scroll => events.push(Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: Vec2::new(pointer.axis_value(android_activity::input::Axis::Hscroll), -pointer.axis_value(android_activity::input::Axis::Vscroll)),
                phase: egui::TouchPhase::Move,
                modifiers: Modifiers::default(),
            }),
            _ => {}
        }
    }

    fn initialize_renderer(
        window: android_activity::ndk::native_window::NativeWindow,
        android: &AndroidApp,
        retained: Option<egui_wgpu::RenderState>,
    ) -> Result<AndroidRenderer, String> {
        let width = u32::try_from(window.width()).unwrap_or(1).max(1);
        let height = u32::try_from(window.height()).unwrap_or(1).max(1);
        let pixels_per_point = android.config().density().map_or(1.0, |density| density as f32 / 160.0).max(1.0);
        let instance = retained.as_ref().map_or_else(
            || {
                wgpu::Instance::new(wgpu::InstanceDescriptor {
                    // Android NativeActivity exposes ANativeWindow; GLES is compatible with
                    // emulator and device drivers that do not expose a Vulkan adapter.
                    backends: wgpu::Backends::GL,
                    flags: wgpu::InstanceFlags::default(),
                    memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
                    backend_options: wgpu::BackendOptions::default(),
                    display: None,
                })
            },
            |state| state.instance.clone(),
        );
        let surface = instance.create_surface(AndroidSurfaceWindow(window)).map_err(|error| error.to_string())?;
        let (adapter, device, queue, renderer, target_format) = if let Some(state) = retained {
            (state.adapter, state.device, state.queue, state.renderer, state.target_format)
        } else {
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            }))
            .map_err(|error| error.to_string())?;
            let limits = wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits());
            let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("PhotoCraft Android device"),
                required_features: wgpu::Features::empty(),
                required_limits: limits,
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                ..Default::default()
            }))
            .map_err(|error| error.to_string())?;
            let target_format = surface.get_default_config(&adapter, width, height).ok_or("Android surface has no supported wgpu format")?.format;
            let renderer = Arc::new(egui::mutex::RwLock::new(Renderer::new(&device, target_format, RendererOptions::default())));
            (adapter, device, queue, renderer, target_format)
        };
        let surface_config = surface.get_default_config(&adapter, width, height).ok_or("Android surface has no supported wgpu format")?;
        if surface_config.format != target_format {
            return Err("Android window surface format changed while resuming".into());
        }
        surface.configure(&device, &surface_config);
        let render_state = egui_wgpu::RenderState {
            adapter,
            available_adapters: Vec::new(),
            instance,
            device,
            queue,
            target_format,
            renderer,
            surface_config: egui_wgpu::SurfaceConfig::LOW_LATENCY,
        };
        Ok(AndroidRenderer { surface, surface_config, render_state, pixels_per_point })
    }

    fn resize_renderer(state: &mut AndroidRenderer, window: &android_activity::ndk::native_window::NativeWindow) {
        let width = u32::try_from(window.width()).unwrap_or(1).max(1);
        let height = u32::try_from(window.height()).unwrap_or(1).max(1);
        state.surface_config.width = width;
        state.surface_config.height = height;
        state.surface.configure(&state.render_state.device, &state.surface_config);
    }

    fn render_frame(state: &mut AndroidRenderer, context: &egui::Context, app: &mut PhotocraftApp, time: f64, events: Vec<egui::Event>) -> bool {
        let size = [state.surface_config.width as f32 / state.pixels_per_point, state.surface_config.height as f32 / state.pixels_per_point];
        let mut raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(size[0], size[1]))),
            time: Some(time),
            predicted_dt: 1.0 / 60.0,
            focused: true,
            events,
            ..Default::default()
        };
        app.prepare_raw_input(context, &mut raw_input);
        let output = context.run_ui(raw_input, |ui| {
            app.frame_logic(ui.ctx());
            app.frame_ui(ui);
        });
        let surface_texture = match state.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture) | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                state.surface.configure(&state.render_state.device, &state.surface_config);
                return false;
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => return false,
            wgpu::CurrentSurfaceTexture::Validation => {
                log::warn!("Android wgpu surface validation error");
                return false;
            }
        };
        let view = surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let screen_descriptor =
            ScreenDescriptor { size_in_pixels: [state.surface_config.width, state.surface_config.height], pixels_per_point: state.pixels_per_point };
        let paint_jobs = context.tessellate(output.shapes, output.pixels_per_point);
        let mut encoder = state.render_state.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("PhotoCraft Android frame") });
        let mut renderer = state.render_state.renderer.write();
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                renderer.update_texture(&state.render_state.device, &state.render_state.queue, *id, delta);
            }
        }
        let command_buffers = renderer.update_buffers(&state.render_state.device, &state.render_state.queue, &mut encoder, &paint_jobs, &screen_descriptor);
        {
            let render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("PhotoCraft Android render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            renderer.render(&mut render_pass.forget_lifetime(), &paint_jobs, &screen_descriptor);
        }
        for id in &output.textures_delta.free {
            renderer.free_texture(id);
        }
        drop(renderer);
        let command_buffers = command_buffers.into_iter().chain(std::iter::once(encoder.finish()));
        state.render_state.queue.submit(command_buffers);
        state.render_state.queue.present(surface_texture);
        true
    }

    fn services(data_dir: &Path, inbox: Inbox) -> Services {
        let request_path = data_dir.join(OPEN_REQUEST);
        let prefs_path = data_dir.join("preferences.json");
        let save_prefs_path = prefs_path.clone();
        let result_inbox = inbox;
        Services {
            import: Some(Box::new(|name: &str, bytes: &[u8]| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| photocraft_io::import(name, bytes)))
                    .map_err(|_| "PhotoCraft stopped an unexpected import error".to_string())?
                    .map(|result| (result.document, result.warnings))
                    .map_err(|error| error.to_string())
            })),
            request_open: Some(Box::new(move || std::fs::write(&request_path, b"open").map_err(|error| error.to_string()))),
            inbox: Some(result_inbox),
            load_prefs: Some(Box::new(move || std::fs::read_to_string(&prefs_path).ok())),
            save_prefs: Some(Box::new(move |text| std::fs::write(&save_prefs_path, text).map_err(|error| error.to_string()))),
            ..Default::default()
        }
    }

    fn receive_picker_result(data_dir: &Path, inbox: &Inbox, context: &egui::Context) {
        let ready = data_dir.join(OPEN_READY);
        if !ready.is_file() {
            return;
        }
        let result_path = data_dir.join(OPEN_RESULT);
        let _ = std::fs::remove_file(&ready);
        let result = std::fs::read(&result_path).map_err(|error| error.to_string()).and_then(parse_picker_payload);
        let _ = std::fs::remove_file(&result_path);
        match result {
            Ok((name, bytes)) => {
                inbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((name, bytes));
                context.request_repaint();
            }
            Err(error) => log::error!("PhotoCraft could not open the selected document: {error}"),
        }
    }
}

/// NativeActivity's glue resolves this exact Rust ABI symbol when starting the library.
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub fn android_main(app: android_activity::AndroidApp) {
    android::android_main(app);
}
