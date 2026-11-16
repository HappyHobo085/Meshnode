// The Kotlin example of crates/mesh-client/README.md, compiled (never run) so that it
// stays in step with the bindings.
@file:Suppress("unused", "UNUSED_VARIABLE")

import net.meshnode.client.*

suspend fun run(dir: String, ticket: String?) {
    val opts = MeshClientOptions(name = "my-app") // platform = this OS, requestTimeoutMs = 30000
    val client = if (ticket != null) MeshClient.join(dir, ticket, opts) else MeshClient.open(dir, opts)
    try {
        client.setListener(object : MeshClientListener {
            override fun onEvent(event: MeshEvent) {
                // MembersChanged, ServicesChanged, Removed, Online, Offline.
                // Called on a library thread: return quickly.
            }
        })
        val resp = client.request(
            "blog",
            MeshRequest(method = "GET", path = "/", headers = listOf(), body = byteArrayOf()),
        )
        println("${resp.status} ${resp.body.decodeToString()}")
        println(client.status())   // mesh, id, onlineHosts, firstSyncDone, removed
        println(client.members())  // id (hex), name, role, platform, online, rttMs
        println(client.services()) // name, kind, hosts
        val invite = client.invite(MeshRole.CLIENT, 600uL)
    } catch (e: MeshClientException.NoHost) {
        // no host could serve it: try again later
    } finally {
        client.shutdown()
    }
}
