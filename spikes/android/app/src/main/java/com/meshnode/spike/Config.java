package com.meshnode.spike;
// THROWAWAY SPIKE: peer id + interval come from files/config.txt ("<peerId> <secs>"), written via adb run-as.
import android.content.Context;
import java.io.File;
import java.nio.file.Files;

final class Config {
    static String[] read(Context c) {
        try {
            String s = new String(Files.readAllBytes(new File(c.getFilesDir(), "config.txt").toPath())).trim();
            return s.split("\\s+");
        } catch (Exception e) {
            return null;
        }
    }
}
