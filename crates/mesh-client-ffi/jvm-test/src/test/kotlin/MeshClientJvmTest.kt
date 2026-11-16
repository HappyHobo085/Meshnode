import java.nio.file.Files
import kotlin.test.Test
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertTrue
import kotlinx.coroutines.runBlocking
import net.meshnode.client.MeshClient
import net.meshnode.client.MeshClientException
import net.meshnode.client.MeshClientOptions

/**
 * The bindings against the host library, without a mesh (sub-spec 8a §6.4): the library
 * loads through JNA, the suspend calls bridge to Rust's async exports, and the Rust errors
 * arrive as the sealed MeshClientException's variants.
 */
class MeshClientJvmTest {
    @Test
    fun joinWithAnInvalidTicketThrowsInvalidTicket() = runBlocking {
        val root = Files.createTempDirectory("mesh-client-jvm")
        val dir = root.resolve("client")
        val e = assertFailsWith<MeshClientException.InvalidTicket> {
            MeshClient.join(dir.toString(), "not a ticket", MeshClientOptions(name = "jvm"))
        }
        assertTrue(e.message!!.contains("ticket"), e.message)
        assertFalse(Files.exists(dir.resolve("node.key")), "nothing written")
    }

    @Test
    fun openOnAnEmptyDirThrowsNotJoined() = runBlocking {
        val dir = Files.createTempDirectory("mesh-client-jvm")
        assertFailsWith<MeshClientException.NotJoined> {
            MeshClient.open(dir.toString(), MeshClientOptions())
        }
        Unit
    }
}
