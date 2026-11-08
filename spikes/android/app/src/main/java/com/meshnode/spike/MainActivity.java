package com.meshnode.spike;
// THROWAWAY SPIKE: asks for notification permission + battery-optimisation exemption, then starts the service.
import android.app.Activity;
import android.content.Intent;
import android.net.Uri;
import android.os.Bundle;
import android.os.PowerManager;
import android.provider.Settings;
import android.widget.TextView;

public class MainActivity extends Activity {
    @Override
    protected void onCreate(Bundle b) {
        super.onCreate(b);
        TextView t = new TextView(this);
        t.setPadding(40, 160, 40, 40);
        setContentView(t);
        if (checkSelfPermission("android.permission.POST_NOTIFICATIONS") != 0) {
            requestPermissions(new String[] {"android.permission.POST_NOTIFICATIONS"}, 1);
        }
        PowerManager pm = getSystemService(PowerManager.class);
        boolean exempt = pm.isIgnoringBatteryOptimizations(getPackageName());
        if (!exempt) {
            startActivity(new Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                    Uri.parse("package:" + getPackageName())));
        }
        String[] cfg = Config.read(this);
        t.setText("Meshnode spike\nconfig: " + (cfg == null ? "MISSING files/config.txt" : String.join(" ", cfg))
                + "\nbattery-opt exempt: " + exempt);
        if (cfg != null) {
            startForegroundService(new Intent(this, MeshService.class).setAction("ui"));
        }
    }
}
