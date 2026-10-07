package ai.storyteller.photocraft;

import android.app.NativeActivity;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.provider.OpenableColumns;
import android.util.Log;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/** Android platform bridge only: egui and all product UI remain in Rust. */
public final class PhotoCraftActivity extends NativeActivity {
    private static final String TAG = "PhotoCraft";
    private static final int PICK_DOCUMENT = 37;
    private static final String REQUEST_FILE = "photocraft-open-request";
    private static final String RESULT_FILE = "photocraft-open-result";
    private static final String READY_FILE = "photocraft-open-ready";
    private static final long MAX_BYTES = 536870912L;

    private final Handler handler = new Handler(Looper.getMainLooper());
    private final ExecutorService documentIo = Executors.newSingleThreadExecutor();
    private boolean resumed;

    private final Runnable pickerPoll = new Runnable() {
        @Override
        public void run() {
            if (resumed) {
                File request = new File(getFilesDir(), REQUEST_FILE);
                if (request.delete()) {
                    Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
                    intent.addCategory(Intent.CATEGORY_OPENABLE);
                    intent.setType("*/*");
                    startActivityForResult(intent, PICK_DOCUMENT);
                }
            }
            handler.postDelayed(this, 250L);
        }
    };

    @Override
    public void onCreate(Bundle state) {
        super.onCreate(state);
        handler.post(pickerPoll);
    }

    @Override
    protected void onResume() {
        super.onResume();
        resumed = true;
        handler.removeCallbacks(pickerPoll);
        handler.post(pickerPoll);
    }

    @Override
    protected void onPause() {
        resumed = false;
        handler.removeCallbacks(pickerPoll);
        super.onPause();
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode != PICK_DOCUMENT || resultCode != RESULT_OK || data == null) {
            return;
        }
        Uri uri = data.getData();
        if (uri == null) {
            return;
        }
        documentIo.execute(new Runnable() {
            @Override
            public void run() {
                copyDocument(uri);
            }
        });
    }

    private void copyDocument(Uri uri) {
        File temporary = new File(getFilesDir(), RESULT_FILE + ".tmp");
        File result = new File(getFilesDir(), RESULT_FILE);
        File readyTemporary = new File(getFilesDir(), READY_FILE + ".tmp");
        File ready = new File(getFilesDir(), READY_FILE);
        try {
            String name = displayName(uri);
            byte[] nameBytes = name.getBytes(StandardCharsets.UTF_8);
            if (nameBytes.length > 4096) {
                throw new IllegalArgumentException("selected file name is too long");
            }
            try (InputStream input = new BufferedInputStream(getContentResolver().openInputStream(uri));
                 DataOutputStream output = new DataOutputStream(new BufferedOutputStream(new FileOutputStream(temporary)))) {
                if (input == null) {
                    throw new IllegalStateException("document provider returned no stream");
                }
                output.writeInt(nameBytes.length);
                output.write(nameBytes);
                byte[] buffer = new byte[65536];
                long total = 0L;
                int count;
                while ((count = input.read(buffer)) != -1) {
                    total += count;
                    if (total > MAX_BYTES) {
                        throw new IllegalArgumentException("selected file exceeds 512 MiB");
                    }
                    output.write(buffer, 0, count);
                }
            }
            if (!temporary.renameTo(result)) {
                throw new IllegalStateException("could not finalize the selected file");
            }
            try (FileOutputStream marker = new FileOutputStream(readyTemporary)) {
                marker.write(1);
                marker.getFD().sync();
            }
            if (!readyTemporary.renameTo(ready)) {
                throw new IllegalStateException("could not notify PhotoCraft about the selected file");
            }
        } catch (Exception error) {
            temporary.delete();
            Log.e(TAG, "Could not read selected document", error);
        }
    }

    @Override
    protected void onDestroy() {
        handler.removeCallbacks(pickerPoll);
        documentIo.shutdown();
        super.onDestroy();
    }

    private String displayName(Uri uri) {
        try (Cursor cursor = getContentResolver().query(uri, null, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                int column = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                if (column >= 0) {
                    String value = cursor.getString(column);
                    if (value != null && !value.trim().isEmpty()) {
                        return value;
                    }
                }
            }
        } catch (Exception error) {
            Log.w(TAG, "Could not read document display name", error);
        }
        return "image";
    }
}
