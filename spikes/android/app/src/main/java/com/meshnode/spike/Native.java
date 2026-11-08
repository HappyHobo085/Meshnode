package com.meshnode.spike;
// THROWAWAY SPIKE
import android.content.Context;

final class Native {
    static { System.loadLibrary("spike"); }
    static native void start(Context ctx, String filesDir, String peerId, int intervalSecs);
    static native void mark(String msg);
}
