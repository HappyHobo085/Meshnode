package com.meshnode.spike;
// THROWAWAY SPIKE: specialUse foreground service hosting the Rust node in-process. Deliberately NO wake lock,
// to measure the worst case of the "Balanced" policy: does the process keep networking with the screen off?
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.Service;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.ServiceInfo;
import android.os.IBinder;
import android.os.PowerManager;

public class MeshService extends Service {
    private BroadcastReceiver marks;

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        NotificationManager nm = getSystemService(NotificationManager.class);
        nm.createNotificationChannel(new NotificationChannel("mesh", "Mesh node", NotificationManager.IMPORTANCE_LOW));
        Notification n = new Notification.Builder(this, "mesh")
                .setContentTitle("Meshnode spike")
                .setContentText("Hosting (spike), pinging desktop")
                .setSmallIcon(android.R.drawable.stat_sys_upload)
                .setOngoing(true)
                .build();
        startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);

        String[] cfg = Config.read(this);
        if (cfg == null || cfg.length < 2) {
            stopSelf();
            return START_NOT_STICKY;
        }
        Native.start(this, getFilesDir().getAbsolutePath(), cfg[0], Integer.parseInt(cfg[1]));
        Native.mark("service onStartCommand flags=" + flags + " action="
                + (intent == null ? "null(sticky restart)" : intent.getAction()));

        if (marks == null) {
            marks = new BroadcastReceiver() {
                @Override
                public void onReceive(Context c, Intent i) {
                    PowerManager pm = c.getSystemService(PowerManager.class);
                    Native.mark(i.getAction() + " idle=" + pm.isDeviceIdleMode()
                            + " interactive=" + pm.isInteractive()
                            + " battOptExempt=" + pm.isIgnoringBatteryOptimizations(getPackageName()));
                }
            };
            IntentFilter f = new IntentFilter();
            f.addAction(Intent.ACTION_SCREEN_OFF);
            f.addAction(Intent.ACTION_SCREEN_ON);
            f.addAction(PowerManager.ACTION_DEVICE_IDLE_MODE_CHANGED);
            f.addAction(Intent.ACTION_POWER_CONNECTED);
            f.addAction(Intent.ACTION_POWER_DISCONNECTED);
            registerReceiver(marks, f, Context.RECEIVER_NOT_EXPORTED);
        }
        return START_STICKY;
    }

    @Override
    public void onTaskRemoved(Intent rootIntent) {
        Native.mark("onTaskRemoved (swiped away)");
    }

    @Override
    public void onDestroy() {
        Native.mark("service onDestroy");
    }

    @Override
    public IBinder onBind(Intent i) {
        return null;
    }
}
