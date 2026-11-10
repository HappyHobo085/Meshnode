package net.meshnode.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.os.BatteryManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import java.io.File
import java.util.concurrent.Executors
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import net.meshnode.ffi.FfiException
import net.meshnode.ffi.FfiPowerPolicy
import net.meshnode.ffi.FfiServiceControl
import net.meshnode.ffi.FfiState
import net.meshnode.ffi.FfiStatus
import net.meshnode.ffi.FfiWakeReason
import net.meshnode.ffi.networkChanged
import net.meshnode.ffi.setPower
import net.meshnode.ffi.setServiceControl
import net.meshnode.ffi.setWakeLockSink
import net.meshnode.ffi.start
import net.meshnode.ffi.status
import net.meshnode.ffi.stop
import net.meshnode.ffi.wake

/**
 * The foreground service (type `specialUse`) that owns the in-process node (spec §5.1).
 *
 * Main-thread state: [startInFlight], [pendingStart], [stopping]. The notification text is
 * kept in [lastText] so that `startForeground` never has to call into mesh-ffi.
 */
class MeshService : Service() {
  private val main = Handler(Looper.getMainLooper())

  private var startInFlight = false
  private var pendingStart = false
  @Volatile private var stopping = false
  @Volatile private var starting = false

  /** Whether the last status showed a node up (any state but `Stopped`). */
  @Volatile private var nodeUp = false
  @Volatile private var foreground = false
  @Volatile private var lastText = NotificationText.starting()

  @Volatile private var charging = false
  @Volatile private var screenOn = true

  private var refreshJob: Job? = null
  private var networkDebounce: Job? = null
  private var networkCallback: ConnectivityManager.NetworkCallback? = null
  private var powerReceiverRegistered = false

  private val powerReceiver = object : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
      when (intent.action) {
        Intent.ACTION_POWER_CONNECTED -> charging = true
        Intent.ACTION_POWER_DISCONNECTED -> charging = false
        Intent.ACTION_SCREEN_ON -> screenOn = true
        Intent.ACTION_SCREEN_OFF -> screenOn = false
        else -> return
      }
      reportPower()
      if (intent.action == Intent.ACTION_SCREEN_ON) scope.launch { refreshNotification() }
    }
  }

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onCreate() {
    super.onCreate()
    try {
      MeshNative.initAndroid(applicationContext)
    } catch (t: Throwable) {
      // A failure inside initAndroid is recorded by Rust and makes start() fail visibly.
      Log.e(TAG, "initAndroid failed", t)
    }
    instance = this
    createChannel()
    try {
      setWakeLockSink(WakeLocks.get(this))
      setServiceControl(ServiceControl(this))
    } catch (t: Throwable) {
      Log.e(TAG, "registering the callbacks failed", t)
    }
    readPowerState()
    registerPowerReceiver()
    registerNetworkCallback()
    reportPower()
    Alarms.scheduleTick(this)
    WatchdogWorker.schedule(this)
    refreshJob = scope.launch {
      while (true) {
        delay(REFRESH_MS)
        if (screenOn) refreshNotification()
      }
    }
  }

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    if (intent?.action == ACTION_STOP) {
      userStop()
      return START_NOT_STICKY
    }
    if (intent == null && Prefs(this).userStopped) {
      // A sticky restart after the user stopped it: stay stopped.
      stopSelf()
      return START_NOT_STICKY
    }
    // Every startForegroundService must be answered by startForeground, also when already
    // in the foreground.
    if (!enterForeground()) {
      stopSelf()
      return START_NOT_STICKY
    }
    startNode()
    return START_STICKY
  }

  override fun onTaskRemoved(rootIntent: Intent?) {
    super.onTaskRemoved(rootIntent)
    if (!Prefs(this).userStopped) Alarms.scheduleRestart(this, RESTART_DELAY_MS)
  }

  override fun onDestroy() {
    if (instance === this) instance = null
    foreground = false
    refreshJob?.cancel()
    networkDebounce?.cancel()
    networkCallback?.let {
      try {
        getSystemService(ConnectivityManager::class.java)?.unregisterNetworkCallback(it)
      } catch (e: RuntimeException) {
        Log.w(TAG, "unregisterNetworkCallback failed", e)
      }
    }
    networkCallback = null
    if (powerReceiverRegistered) {
      unregisterReceiver(powerReceiver)
      powerReceiverRegistered = false
    }
    super.onDestroy()
  }

  /** Whether the node is running in this process (a `Stopped` status is not running). */
  fun isNodeRunning(): Boolean =
    try {
      status().state != FfiState.STOPPED
    } catch (t: Throwable) {
      false
    }

  /** Starts the node (idempotent in Rust); a Stop in progress defers it until done. */
  private fun startNode() {
    if (stopping) {
      pendingStart = true
      return
    }
    if (startInFlight) return
    startInFlight = true
    starting = true
    if (!nodeUp) post(NotificationText.starting())
    val dir = dataDir(this)
    scope.launch {
      var text: String? = null
      var stopWon = false
      try {
        dir.mkdirs()
        val s = start(dir.path)
        if (s.state == FfiState.STOPPED) {
          // A later stop() won: stopped, not running.
          stopWon = true
          nodeUp = false
        } else {
          text = textFor(s)
          reportPower() // now with the policy from meshd.toml
        }
      } catch (e: CancellationException) {
        throw e
      } catch (e: FfiException) {
        nodeUp = false
        text = NotificationText.failed(e.message ?: e.javaClass.simpleName)
      } catch (t: Throwable) {
        nodeUp = false
        text = NotificationText.failed(t.message ?: t.javaClass.simpleName)
        Log.e(TAG, "start failed", t)
      }
      main.post { onStartDone(text, stopWon) }
    }
  }

  private fun onStartDone(text: String?, stopWon: Boolean) {
    startInFlight = false
    starting = false
    if (text != null) post(text)
    // A stop that won is followed by Start only if the user pressed Start meanwhile.
    if (stopWon && !stopping && !Prefs(this).userStopped) startNode()
  }

  /** The Stop action: `stop()`, persist `user_stopped`, leave the foreground, `stopSelf`. */
  fun userStop() {
    Prefs(this).userStopped = true
    pendingStart = false
    if (stopping) return
    stopping = true
    scope.launch {
      try {
        stop()
      } catch (e: CancellationException) {
        throw e
      } catch (t: Throwable) {
        Log.e(TAG, "stop failed", t)
      }
      nodeUp = false
      main.post { finishStop() }
    }
  }

  private fun finishStop() {
    stopping = false
    if (pendingStart) {
      // Start was pressed while stopping.
      pendingStart = false
      startNode()
      return
    }
    foreground = false
    ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
    stopSelf()
  }

  /** Re-reads the status and updates the notification (any thread). */
  fun refreshNotification() {
    if (stopping || (starting && !nodeUp)) return
    val s = try {
      status()
    } catch (t: Throwable) {
      Log.w(TAG, "status failed", t)
      return
    }
    post(textFor(s))
  }

  private fun textFor(s: FfiStatus): String {
    val phase = when (s.state) {
      FfiState.RUNNING, FfiState.REBOOTSTRAPPING -> NodePhase.RUNNING
      FfiState.UNINITIALISED, FfiState.REMOVED -> NodePhase.NOT_SET_UP
      FfiState.STOPPED -> NodePhase.FAILED
    }
    nodeUp = s.state != FfiState.STOPPED
    return NotificationText.forPhase(
      phase,
      s.servicesHosted.toInt(),
      s.peersOnline.toInt(),
      s.lastError,
    )
  }

  private fun post(text: String) {
    lastText = text
    if (!foreground || stopping) return
    try {
      getSystemService(NotificationManager::class.java)?.notify(NOTIFICATION_ID, build(text))
    } catch (e: RuntimeException) {
      Log.w(TAG, "notify failed", e)
    }
  }

  private fun enterForeground(): Boolean =
    try {
      val n = build(lastText)
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
        startForeground(NOTIFICATION_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
      } else {
        startForeground(NOTIFICATION_ID, n)
      }
      foreground = true
      true
    } catch (e: RuntimeException) {
      // ForegroundServiceStartNotAllowedException (a background start) or a SecurityException.
      Log.e(TAG, "startForeground refused", e)
      false
    }

  private fun build(text: String): Notification {
    val open = PendingIntent.getActivity(
      this,
      0,
      Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
    val stopIntent = PendingIntent.getService(
      this,
      1,
      Intent(this, MeshService::class.java).setAction(ACTION_STOP),
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
    return NotificationCompat.Builder(this, CHANNEL_ID)
      .setSmallIcon(R.drawable.ic_stat_meshnode)
      .setContentText(text)
      .setStyle(NotificationCompat.BigTextStyle().bigText(text))
      .setContentIntent(open)
      .addAction(0, "Stop", stopIntent)
      .addAction(0, "Open", open)
      .setOngoing(true)
      .setOnlyAlertOnce(true)
      .setShowWhen(false)
      .setCategory(NotificationCompat.CATEGORY_SERVICE)
      .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
      .build()
  }

  private fun createChannel() {
    val channel = NotificationChannel(CHANNEL_ID, "Meshnode", NotificationManager.IMPORTANCE_LOW)
    channel.setShowBadge(false)
    getSystemService(NotificationManager::class.java)?.createNotificationChannel(channel)
  }

  private fun readPowerState() {
    screenOn = getSystemService(PowerManager::class.java)?.isInteractive ?: true
    val battery = ContextCompat.registerReceiver(
      this,
      null,
      IntentFilter(Intent.ACTION_BATTERY_CHANGED),
      ContextCompat.RECEIVER_NOT_EXPORTED,
    )
    charging = (battery?.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) ?: 0) != 0
  }

  private fun registerPowerReceiver() {
    val filter = IntentFilter().apply {
      addAction(Intent.ACTION_POWER_CONNECTED)
      addAction(Intent.ACTION_POWER_DISCONNECTED)
      addAction(Intent.ACTION_SCREEN_ON)
      addAction(Intent.ACTION_SCREEN_OFF)
    }
    // System broadcasts reach a not-exported receiver.
    ContextCompat.registerReceiver(this, powerReceiver, filter, ContextCompat.RECEIVER_NOT_EXPORTED)
    powerReceiverRegistered = true
  }

  /**
   * Reports charging and screen state with the policy in force (`status().powerPolicy`,
   * from meshd.toml), so it is never changed here. Before a start the policy is ignored.
   * Serialised, so the last report carries the latest state.
   */
  private fun reportPower() {
    powerExecutor.execute {
      try {
        val policy = status().powerPolicy ?: FfiPowerPolicy.BALANCED
        setPower(policy, charging, screenOn)
      } catch (t: Throwable) {
        Log.w(TAG, "setPower failed", t)
      }
    }
  }

  private fun registerNetworkCallback() {
    val cm = getSystemService(ConnectivityManager::class.java) ?: return
    val cb = object : ConnectivityManager.NetworkCallback() {
      override fun onAvailable(network: Network) = onNetworkChange()
      override fun onLost(network: Network) = onNetworkChange()
      override fun onLinkPropertiesChanged(network: Network, linkProperties: LinkProperties) =
        onNetworkChange()
    }
    try {
      cm.registerDefaultNetworkCallback(cb)
      networkCallback = cb
    } catch (e: RuntimeException) {
      Log.w(TAG, "registerDefaultNetworkCallback failed", e)
    }
  }

  /** Coalesces a burst of callbacks (one switch fires several) into one rebind and wake. */
  private fun onNetworkChange() {
    synchronized(this) {
      networkDebounce?.cancel()
      networkDebounce = scope.launch {
        delay(NETWORK_DEBOUNCE_MS)
        // Not cancelled by a later callback once started.
        scope.launch {
          try {
            networkChanged()
            wake(FfiWakeReason.NETWORK)
          } catch (e: CancellationException) {
            throw e
          } catch (t: Throwable) {
            Log.w(TAG, "network wake failed", t)
          }
          refreshNotification()
        }
      }
    }
  }

  companion object {
    const val ACTION_STOP = "net.meshnode.app.action.STOP"
    private const val TAG = "Meshnode"
    private const val CHANNEL_ID = "meshnode"
    private const val NOTIFICATION_ID = 1
    private const val REFRESH_MS = 30_000L
    private const val RESTART_DELAY_MS = 5_000L
    private const val NETWORK_DEBOUNCE_MS = 1_000L

    /** Process-wide scope for calls into mesh-ffi (its async functions are `suspend`). */
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    private val powerExecutor = Executors.newSingleThreadExecutor()

    @Volatile
    var instance: MeshService? = null
      private set

    fun dataDir(context: Context): File = File(context.filesDir, "meshnode")

    /**
     * Starts the service if [RestartPolicy] allows it for [trigger]. "Running" means the
     * node runs in this process; with no service instance it cannot.
     */
    fun startIfAllowed(context: Context, trigger: RestartPolicy.Trigger): Boolean {
      val app = context.applicationContext
      val prefs = Prefs(app)
      if (RestartPolicy.clearsUserStopped(trigger)) prefs.userStopped = false
      val running = instance?.isNodeRunning() ?: false
      if (!RestartPolicy.shouldStart(trigger, prefs.userStopped, running)) return false
      return launch(app)
    }

    /** `startForegroundService`; false when Android refuses it (e.g. a background start). */
    fun launch(context: Context): Boolean =
      try {
        ContextCompat.startForegroundService(context, Intent(context, MeshService::class.java))
        true
      } catch (e: RuntimeException) {
        Log.w(TAG, "startForegroundService refused", e)
        false
      }
  }
}

/**
 * What Tauri's `service_start` / `service_stop` commands call. Never throws into Rust and
 * never calls back into mesh-ffi synchronously.
 */
class ServiceControl(context: Context) : FfiServiceControl {
  private val app = context.applicationContext
  private val main = Handler(Looper.getMainLooper())

  /** Start pressed: clear `user_stopped` and start the service. */
  override fun startService() {
    try {
      Prefs(app).userStopped = false
      MeshService.launch(app)
    } catch (t: Throwable) {
      Log.e("Meshnode", "service_start failed", t)
    }
  }

  /** Stop pressed: set `user_stopped` and stop the service (and the node). */
  override fun stopService() {
    try {
      Prefs(app).userStopped = true
      main.post {
        val service = MeshService.instance
        if (service != null) {
          service.userStop()
        } else {
          MeshService.scope.launch {
            try {
              stop()
            } catch (t: Throwable) {
              Log.e("Meshnode", "stop failed", t)
            }
          }
        }
      }
    } catch (t: Throwable) {
      Log.e("Meshnode", "service_stop failed", t)
    }
  }
}
