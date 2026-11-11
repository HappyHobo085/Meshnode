package net.meshnode.app

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.util.Base64
import androidx.activity.result.ActivityResult
import androidx.core.net.toUri
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.io.InputStream
import java.util.concurrent.Executors

@InvokeArg
class TreeArgs {
  lateinit var treeUri: String
}

@InvokeArg
class ReadArgs {
  lateinit var uri: String
  var offset: Long = 0
  var len: Int = 0
}

@InvokeArg
class PickFileArgs {
  var mime: String = "*/*"
}

/**
 * The Storage Access Framework for the Rust side (spec §7.2): pick a document tree and list
 * and read its files (a static site deploy), or pick one file and copy it to the app cache
 * (a `.wasm` deploy). Listing, reading and copying run off the main thread.
 */
@TauriPlugin
class FolderPickerPlugin(private val activity: Activity) : Plugin(activity) {
  private val io = Executors.newSingleThreadExecutor()

  /** `ACTION_OPEN_DOCUMENT_TREE`; resolves `{uri}` (absent when cancelled). */
  @Command
  fun pickTree(invoke: Invoke) {
    val intent = Intent(Intent.ACTION_OPEN_DOCUMENT_TREE)
      .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION)
    startActivityForResult(invoke, intent, "pickTreeResult")
  }

  @ActivityCallback
  fun pickTreeResult(invoke: Invoke, result: ActivityResult) {
    val uri = result.data?.data
    val out = JSObject()
    if (result.resultCode == Activity.RESULT_OK && uri != null) {
      // Held for the deploy only: `release` gives it back.
      try {
        activity.contentResolver.takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION)
      } catch (_: SecurityException) {
        // The one-time grant still covers this session.
      }
      out.put("uri", uri.toString())
    }
    invoke.resolve(out)
  }

  /** Gives back the persisted read grant taken by `pickTree`. */
  @Command
  fun release(invoke: Invoke) {
    val args = invoke.parseArgs(TreeArgs::class.java)
    try {
      activity.contentResolver.releasePersistableUriPermission(
        args.treeUri.toUri(), Intent.FLAG_GRANT_READ_URI_PERMISSION
      )
    } catch (_: SecurityException) {
      // Not persisted (or already released).
    }
    invoke.resolve()
  }

  /**
   * Every file under the tree: resolves `{files: [{path, size, uri}]}`, `path` relative to the
   * tree root (see [SafPaths]). Rejects a name that cannot be part of an upload path, and stops
   * past [MAX_FILES] files.
   */
  @Command
  fun list(invoke: Invoke) {
    val args = invoke.parseArgs(TreeArgs::class.java)
    io.execute {
      try {
        val tree = args.treeUri.toUri()
        val files = JSArray()
        var count = 0
        // (document id, display names from the root)
        val pending = ArrayDeque<Pair<String, List<String>>>()
        pending.add(DocumentsContract.getTreeDocumentId(tree) to emptyList())
        while (pending.isNotEmpty()) {
          val (dirId, names) = pending.removeFirst()
          val children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, dirId)
          val columns = arrayOf(
            Document.COLUMN_DOCUMENT_ID,
            Document.COLUMN_DISPLAY_NAME,
            Document.COLUMN_MIME_TYPE,
            Document.COLUMN_SIZE,
          )
          activity.contentResolver.query(children, columns, null, null, null)?.use { c ->
            while (c.moveToNext()) {
              val id = c.getString(0)
              val name = c.getString(1) ?: ""
              val segs = names + name
              val path = SafPaths.relative(segs)
                ?: throw IllegalArgumentException(
                  "${segs.joinToString("/")} has a name that cannot be uploaded"
                )
              if (c.getString(2) == Document.MIME_TYPE_DIR) {
                pending.add(id to segs)
              } else {
                count += 1
                if (count > MAX_FILES) {
                  throw IllegalArgumentException("The folder has more than $MAX_FILES files")
                }
                val size = if (c.isNull(3)) -1L else c.getLong(3)
                val f = JSObject()
                f.put("path", path)
                f.put("size", size)
                f.put("uri", DocumentsContract.buildDocumentUriUsingTree(tree, id).toString())
                files.put(f)
              }
            }
          } ?: throw IllegalStateException("The folder cannot be read")
        }
        val out = JSObject()
        out.put("files", files)
        invoke.resolve(out)
      } catch (e: IllegalArgumentException) {
        invoke.reject(e.message, INVALID)
      } catch (e: Exception) {
        invoke.reject(e.message ?: e.toString())
      }
    }
  }

  /**
   * At most `len` bytes of a document from `offset`: resolves `{data}` (base64; shorter than
   * `len` only at the end). Sequential reads of one document reuse its open stream.
   */
  @Command
  fun read(invoke: Invoke) {
    val args = invoke.parseArgs(ReadArgs::class.java)
    io.execute {
      try {
        val len = args.len.coerceIn(0, MAX_CHUNK)
        val buf = ByteArray(len)
        val input = cursor(args.uri, args.offset)
        var n = 0
        while (n < len) {
          val r = input.read(buf, n, len - n)
          if (r < 0) break
          n += r
        }
        reading?.let { it.position += n }
        if (n < len) closeCursor()
        val out = JSObject()
        out.put("data", Base64.encodeToString(buf, 0, n, Base64.NO_WRAP))
        invoke.resolve(out)
      } catch (e: Exception) {
        closeCursor()
        invoke.reject(e.message ?: e.toString())
      }
    }
  }

  /** The open document being read (used on the `io` thread only). */
  private class Reading(val uri: String, val input: InputStream, var position: Long)

  private var reading: Reading? = null

  private fun cursor(uri: String, offset: Long): InputStream {
    val r = reading
    if (r != null && r.uri == uri && r.position == offset) return r.input
    closeCursor()
    val input = open(uri.toUri())
    reading = Reading(uri, input, 0)
    skipFully(input, offset)
    reading?.position = offset
    return input
  }

  private fun closeCursor() {
    try {
      reading?.input?.close()
    } catch (_: Exception) {
    }
    reading = null
  }

  /** `ACTION_OPEN_DOCUMENT` for one file of type `mime`; resolves `{path}` of its copy in the cache. */
  @Command
  fun pickFile(invoke: Invoke) {
    val args = invoke.parseArgs(PickFileArgs::class.java)
    val intent = Intent(Intent.ACTION_OPEN_DOCUMENT)
      .addCategory(Intent.CATEGORY_OPENABLE)
      .setType("*/*")
      // Many providers do not know `.wasm` and report a generic binary type.
      .putExtra(Intent.EXTRA_MIME_TYPES, arrayOf(args.mime, "application/octet-stream"))
    startActivityForResult(invoke, intent, "pickFileResult")
  }

  @ActivityCallback
  fun pickFileResult(invoke: Invoke, result: ActivityResult) {
    val uri = result.data?.data
    if (result.resultCode != Activity.RESULT_OK || uri == null) {
      invoke.resolve(JSObject())
      return
    }
    io.execute {
      try {
        val dir = File(activity.cacheDir, "picked")
        dir.deleteRecursively()
        dir.mkdirs()
        val dest = File(dir, "picked.wasm")
        var total = 0L
        open(uri).use { input ->
          dest.outputStream().use { output ->
            val buf = ByteArray(64 * 1024)
            while (true) {
              val r = input.read(buf)
              if (r < 0) break
              total += r
              if (total > MAX_FILE) {
                throw IllegalArgumentException("The file is larger than 64 MiB")
              }
              output.write(buf, 0, r)
            }
          }
        }
        val out = JSObject()
        out.put("path", dest.absolutePath)
        invoke.resolve(out)
      } catch (e: Exception) {
        File(activity.cacheDir, "picked").deleteRecursively()
        invoke.reject(e.message ?: e.toString(), if (e is IllegalArgumentException) INVALID else null)
      }
    }
  }

  private fun open(uri: Uri): InputStream =
    activity.contentResolver.openInputStream(uri) ?: throw IllegalStateException("$uri cannot be opened")

  private fun skipFully(input: InputStream, count: Long) {
    var left = count
    while (left > 0) {
      val s = input.skip(left)
      if (s > 0) {
        left -= s
      } else if (input.read() < 0) {
        return
      } else {
        left -= 1
      }
    }
  }

  companion object {
    /** The reject code for what breaks the upload rules (an invalid argument on the Rust side). */
    const val INVALID = "invalid"
    /** mesh-admin's `MAX_UPLOAD_FILES`. */
    const val MAX_FILES = 100_000
    /** mesh-admin's `MAX_UPLOAD_FILE` (64 MiB). */
    const val MAX_FILE = 64L * 1024 * 1024
    /** The largest `read` (the Rust side asks for 1 MiB). */
    const val MAX_CHUNK = 4 * 1024 * 1024
  }
}
