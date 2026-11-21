package kr.chronos.sword;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.ActivityNotFoundException;
import android.content.Intent;
import android.net.Uri;

import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.lang.ref.WeakReference;
import java.util.TimeZone;

/**
 * The phone's own document screens behind 환경설정's 세이브 내보내기 /
 * 불러오기 rows (cs_core::savexfer).
 *
 * <p>The native side prepares the bundle as a file in the app's storage and
 * asks for it to be copied out ({@link #export}), or asks for a document to be
 * copied in ({@link #pick}); the answer comes back through
 * {@link #nativeDocResult}. Storage Access Framework, so no permission is
 * asked for and the player can choose any provider the phone has — the
 * Downloads folder, Google Drive, a USB stick.
 *
 * <p>Copies run on a thread of their own: a cloud provider may download inside
 * {@code openInputStream}, and that must not sit on the UI thread. So a copy
 * can outlive its screen, and the game is back — its rows pressable — while
 * it runs. Every request therefore carries the native side's number, through
 * its screen's request code and into its copy thread, and is answered
 * <b>exactly once</b> with that number: the native side takes an answer only
 * for the request it names, and each request has a file of its own.
 */
final class CsSave {
    /** Must match {@code cs_core::savexfer::DocStatus::from_code}. */
    static final int DONE = 0;
    static final int CANCELLED = 1;
    static final int FAILED = 2;

    /** A request code is a kind in the top nibble and the request's number
     *  in the low twelve bits — sixteen bits in all, the most an
     *  AppCompatActivity accepts — so a screen's answer can be matched to the
     *  request that opened it. */
    private static final int REQ_EXPORT = 0x4000;
    private static final int REQ_IMPORT = 0x5000;
    private static final int REQ_KIND = 0xf000;
    private static final int REQ_ID = 0x0fff;
    /** One more than {@code cs_core::savebundle::BUNDLE_MAX}: the native
     *  side refuses anything that size, without this side reading a whole
     *  stranger's file into the app. */
    private static final long IMPORT_CAP = (4L << 20) + 1;

    private static WeakReference<Activity> activity = new WeakReference<>(null);

    /** The request whose screen is open. UI thread only. */
    private static final class Pending {
        final int id;
        final String path;
        final int code;

        Pending(int id, String path, int kind) {
            this.id = id;
            this.path = path;
            this.code = kind | (id & REQ_ID);
        }
    }

    private static Pending pending;

    private CsSave() {}

    /** MainActivity, from onCreate. */
    static void attach(Activity a) {
        activity = new WeakReference<>(a);
    }

    /** For the dates in the native side's popups and the suggested name. */
    static int utcOffsetMinutes() {
        return TimeZone.getDefault().getOffset(System.currentTimeMillis()) / 60000;
    }

    /** A message about the save the native side applied at the start
     *  ({@code cs_core::savebundle::Outcome}). {@code fatal} is a start the
     *  native side will not boot: the dialog cannot be dismissed and its one
     *  button closes the app, so the next launch can try again. False if there
     *  is no activity to show it on; the native side then closes by itself. */
    static boolean notice(final String title, final String message, final boolean fatal) {
        final Activity a = activity.get();
        if (a == null) {
            return false;
        }
        a.runOnUiThread(() -> {
            AlertDialog.Builder b = new AlertDialog.Builder(a)
                    .setTitle(title)
                    .setMessage(message)
                    .setCancelable(!fatal);
            if (fatal) {
                b.setPositiveButton("닫기", (d, which) -> close(a));
            } else {
                b.setPositiveButton("확인", null);
            }
            try {
                b.show();
            } catch (RuntimeException e) {
                // A window that cannot be shown leaves a fatal start on a black
                // screen with nothing to press; closing is better.
                android.util.Log.w("chronos", "save notice failed: " + e);
                if (fatal) {
                    close(a);
                }
            }
        });
        return true;
    }

    /** End the process, not just the activity: the native side never returns
     *  from a blocked start, and a surviving process cannot start it again. */
    private static void close(Activity a) {
        a.finishAndRemoveTask();
        System.exit(0);
    }

    /** Offer "save as" for {@code path}, request {@code id}, suggesting
     *  {@code name}. False if there is no activity to ask from; the native side
     *  then answers the request as failed itself. */
    static boolean export(final String path, final String name, final int id) {
        final Activity a = activity.get();
        if (a == null) {
            return false;
        }
        a.runOnUiThread(() -> {
            Intent i = new Intent(Intent.ACTION_CREATE_DOCUMENT);
            i.addCategory(Intent.CATEGORY_OPENABLE);
            // Not application/zip: some providers append .zip to the name.
            i.setType("application/octet-stream");
            i.putExtra(Intent.EXTRA_TITLE, name);
            open(a, i, new Pending(id, path, REQ_EXPORT));
        });
        return true;
    }

    /** Offer "open" for request {@code id}, and copy what is picked to
     *  {@code path}. */
    static boolean pick(final String path, final int id) {
        final Activity a = activity.get();
        if (a == null) {
            return false;
        }
        a.runOnUiThread(() -> {
            Intent i = new Intent(Intent.ACTION_OPEN_DOCUMENT);
            i.addCategory(Intent.CATEGORY_OPENABLE);
            // A .cssave has no type a provider knows; the native side checks
            // the contents instead.
            i.setType("*/*");
            open(a, i, new Pending(id, path, REQ_IMPORT));
        });
        return true;
    }

    /** UI thread. */
    private static void open(Activity a, Intent i, Pending p) {
        if (pending != null) {
            // The native side makes a new request only once the last one has
            // been answered or given up on, and the game is paused while a
            // screen is up; so a screen still pending here is one whose
            // answer was lost. It gets its one answer now.
            android.util.Log.w("chronos", "save document request " + pending.id + " was never answered");
            nativeDocResult(pending.id, FAILED);
        }
        pending = p;
        try {
            a.startActivityForResult(i, p.code);
        } catch (ActivityNotFoundException e) {
            pending = null;
            nativeDocResult(p.id, FAILED);
        }
    }

    /** From MainActivity.onActivityResult. True if the request was ours. */
    static boolean onActivityResult(final Activity a, int requestCode, int resultCode, Intent data) {
        int kind = requestCode & REQ_KIND;
        if ((kind != REQ_EXPORT && kind != REQ_IMPORT) || (requestCode & ~(REQ_KIND | REQ_ID)) != 0) {
            return false;
        }
        final Pending p = pending;
        if (p == null || p.code != requestCode) {
            // A screen whose request was already answered as lost: nothing to
            // attribute it to.
            android.util.Log.w("chronos", "save document screen " + requestCode + " answered with nothing waiting");
            return true;
        }
        pending = null;
        final Uri uri = data != null ? data.getData() : null;
        if (resultCode != Activity.RESULT_OK || uri == null) {
            nativeDocResult(p.id, CANCELLED);
            return true;
        }
        final boolean export = kind == REQ_EXPORT;
        new Thread(() -> {
            int status = FAILED;
            try {
                if (export) {
                    try (InputStream in = new FileInputStream(p.path);
                         OutputStream out = a.getContentResolver().openOutputStream(uri, "wt")) {
                        if (out == null) {
                            throw new IOException("no stream for " + uri);
                        }
                        copy(in, out, Long.MAX_VALUE);
                    }
                } else {
                    try (InputStream in = a.getContentResolver().openInputStream(uri);
                         OutputStream out = new FileOutputStream(p.path)) {
                        if (in == null) {
                            throw new IOException("no stream for " + uri);
                        }
                        copy(in, out, IMPORT_CAP);
                    }
                }
                status = DONE;
            } catch (Exception e) {
                // Anything at all: the native side waits for this answer, and
                // a thread that died without one would hold the rows.
                android.util.Log.w("chronos", "save document copy " + p.id + " failed: " + e);
            } finally {
                if (export) {
                    // The native side removes its files too; this is the copy's
                    // own file, done with either way.
                    new File(p.path).delete();
                }
                nativeDocResult(p.id, status);
            }
        }, "cs-save-doc").start();
        return true;
    }

    private static void copy(InputStream in, OutputStream out, long cap) throws IOException {
        byte[] buf = new byte[64 * 1024];
        long total = 0;
        int n;
        while (total < cap && (n = in.read(buf, 0, (int) Math.min(buf.length, cap - total))) > 0) {
            out.write(buf, 0, n);
            total += n;
        }
        out.flush();
    }

    /** Implemented in Rust — lib.rs, `Java_kr_chronos_sword_CsSave_nativeDocResult`.
     *  Any thread; {@code id} is the request's number. */
    private static native void nativeDocResult(int id, int status);
}
