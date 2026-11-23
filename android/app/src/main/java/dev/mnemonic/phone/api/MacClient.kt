package dev.mnemonic.phone.api

import java.io.IOException
import java.net.UnknownServiceException
import javax.net.ssl.SSLHandshakeException
import javax.net.ssl.SSLPeerUnverifiedException
import java.util.concurrent.TimeUnit
import kotlin.coroutines.resume
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.serialization.DeserializationStrategy
import kotlinx.serialization.builtins.ListSerializer
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import okhttp3.Call
import okhttp3.Callback
import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrl
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response

sealed interface MacResult<out T> {
    data class Ok<T>(val value: T) : MacResult<T>
    data class Refused(val status: Int, val reason: String) : MacResult<Nothing>
    data class Unreachable(val cause: String, val addressUnusable: Boolean = false) : MacResult<Nothing>
}

class MacClient(private val address: MacAddress, private val http: OkHttpClient) {
    // The Mac waits up to 60 s for a new task or merge before it answers.
    private val postClient: OkHttpClient by lazy {
        http.newBuilder().readTimeout(POST_READ_TIMEOUT_SECONDS, TimeUnit.SECONDS).build()
    }

    suspend fun workspaces(): MacResult<TeamWorkspaceList> =
        get(url("v1", "workspaces"), serializer = TeamWorkspaceList.serializer())

    suspend fun chatUpdate(chatId: String, cursor: String?, waitSeconds: Int): MacResult<TeamChatUpdate> {
        val client = http.newBuilder().readTimeout(waitSeconds + 10L, TimeUnit.SECONDS).build()
        val url = address.baseUrl.toHttpUrl().newBuilder()
            .addPathSegments("v1/chats").addPathSegment(chatId)
            .addQueryParameter("wait", waitSeconds.toString())
            .apply { if (cursor != null) addQueryParameter("cursor", cursor) }
            .build()
        return run(client, Request.Builder().url(url).build(), TeamChatUpdate.serializer())
    }

    suspend fun diff(workspaceId: String): MacResult<String> =
        text(Request.Builder().url(url("v1", "workspaces", workspaceId, "diff")).build())

    suspend fun prStatus(workspaceId: String): MacResult<PhonePrStatus> =
        get(url("v1", "workspaces", workspaceId, "pr"), PhonePrStatus.serializer())

    suspend fun agents(): MacResult<PhoneAgentList> =
        get(url("v1", "agents"), serializer = PhoneAgentList.serializer())

    suspend fun chatOptions(chatId: String): MacResult<PhoneChatOptionList> =
        get(url("v1", "chats", chatId, "options"), PhoneChatOptionList.serializer())

    suspend fun setChatOptions(chatId: String, choices: List<PhoneOptionChoice>): MacResult<PhoneActionOutcome> =
        action(url("v1", "chats", chatId, "options"), buildJsonObject { put("options", encode(choices)) })

    suspend fun repositories(): MacResult<PhoneRepositoryList> =
        get(url("v1", "repositories"), serializer = PhoneRepositoryList.serializer())

    suspend fun sendMessage(chatId: String, text: String): MacResult<PhoneActionOutcome> =
        action(url("v1", "chats", chatId, "message"), "text" to text)

    suspend fun stop(chatId: String): MacResult<PhoneActionOutcome> =
        action(url("v1", "chats", chatId, "stop"))

    suspend fun answerPermission(chatId: String, toolCallId: String, optionId: String): MacResult<PhoneActionOutcome> =
        action(url("v1", "chats", chatId, "permission"), "tool_call_id" to toolCallId, "option_id" to optionId)

    suspend fun newChat(workspaceId: String, agent: String): MacResult<PhoneActionOutcome> =
        action(url("v1", "workspaces", workspaceId, "chats"), "agent" to agent)

    suspend fun startTask(
        repositoryId: String,
        agent: String,
        text: String,
        choices: List<PhoneOptionChoice>,
    ): MacResult<PhoneActionOutcome> =
        action(
            url("v1", "tasks"),
            buildJsonObject {
                put("repository_id", JsonPrimitive(repositoryId))
                put("agent", JsonPrimitive(agent))
                put("text", JsonPrimitive(text))
                put("options", encode(choices))
            },
        )

    suspend fun createPr(workspaceId: String): MacResult<PhoneActionOutcome> =
        action(url("v1", "workspaces", workspaceId, "pr"))

    suspend fun merge(workspaceId: String, head: String): MacResult<PhoneActionOutcome> =
        action(url("v1", "workspaces", workspaceId, "merge"), "head" to head)

    suspend fun archiveWorkspace(workspaceId: String): MacResult<PhoneActionOutcome> =
        action(url("v1", "workspaces", workspaceId, "archive"))

    suspend fun registerDevice(
        installId: String,
        token: String,
        p256dh: String,
        auth: String,
        name: String,
    ): MacResult<Unit> =
        post(
            url("v1", "push", "devices"),
            "install_id" to installId,
            "token" to token,
            "p256dh" to p256dh,
            "auth" to auth,
            "name" to name,
        ) { MacResult.Ok(Unit) }

    suspend fun sendTestAlert(): MacResult<Unit> = post(url("v1", "push", "test")) { MacResult.Ok(Unit) }

    private fun url(vararg segments: String): HttpUrl =
        address.baseUrl.toHttpUrl().newBuilder().apply {
            segments.forEach { addPathSegment(it) }
        }.build()

    private suspend fun <T> get(target: HttpUrl, serializer: DeserializationStrategy<T>): MacResult<T> =
        run(http, Request.Builder().url(target).build(), serializer)

    private suspend fun text(request: Request): MacResult<String> =
        when (val outcome = execute(http, request)) {
            is Reply.Failed -> outcome.result
            is Reply.Body -> MacResult.Ok(outcome.text)
        }

    private fun encode(choices: List<PhoneOptionChoice>): JsonElement =
        MacJson.encodeToJsonElement(ListSerializer(PhoneOptionChoice.serializer()), choices)

    private suspend fun action(target: HttpUrl, vararg fields: Pair<String, String>): MacResult<PhoneActionOutcome> =
        action(target, fields(*fields))

    private suspend fun action(target: HttpUrl, json: JsonObject): MacResult<PhoneActionOutcome> =
        post(target, json) { body ->
            decode(body, PhoneActionOutcome.serializer())
        }

    private fun fields(vararg fields: Pair<String, String>): JsonObject =
        buildJsonObject { fields.forEach { (key, value) -> put(key, JsonPrimitive(value)) } }

    private suspend fun <T> post(
        target: HttpUrl,
        vararg fields: Pair<String, String>,
        read: (String) -> MacResult<T>,
    ): MacResult<T> = post(target, fields(*fields), read)

    private suspend fun <T> post(
        target: HttpUrl,
        json: JsonObject,
        read: (String) -> MacResult<T>,
    ): MacResult<T> {
        val request = Request.Builder()
            .url(target)
            .header("X-Mnemonic-Phone", "app")
            .header("Origin", address.origin)
            .post(json.toString().toRequestBody("application/json".toMediaType()))
            .build()
        return when (val outcome = execute(postClient, request)) {
            is Reply.Failed -> outcome.result
            is Reply.Body -> read(outcome.text)
        }
    }

    private suspend fun <T> run(
        client: OkHttpClient,
        request: Request,
        serializer: DeserializationStrategy<T>,
    ): MacResult<T> =
        when (val outcome = execute(client, request)) {
            is Reply.Failed -> outcome.result
            is Reply.Body -> decode(outcome.text, serializer)
        }

    private fun <T> decode(body: String, serializer: DeserializationStrategy<T>): MacResult<T> =
        try {
            MacResult.Ok(MacJson.decodeFromString(serializer, body))
        } catch (error: IllegalArgumentException) {
            MacResult.Refused(200, "The Mac's answer couldn't be read: ${error.message}")
        }

    private sealed interface Reply {
        data class Body(val text: String) : Reply
        data class Failed(val result: MacResult<Nothing>) : Reply
    }

    private suspend fun execute(client: OkHttpClient, request: Request): Reply =
        suspendCancellableCoroutine { continuation ->
            val call = client.newCall(request)
            continuation.invokeOnCancellation { call.cancel() }
            call.enqueue(object : Callback {
                override fun onFailure(call: Call, e: IOException) {
                    val unusable = e is UnknownServiceException || e is SSLHandshakeException ||
                        e is SSLPeerUnverifiedException
                    continuation.resume(
                        Reply.Failed(MacResult.Unreachable(e.message ?: e.javaClass.simpleName, unusable)),
                    )
                }

                override fun onResponse(call: Call, response: Response) {
                    val reply = response.use {
                        try {
                            val text = it.body?.string().orEmpty()
                            if (it.isSuccessful) Reply.Body(text) else Reply.Failed(MacResult.Refused(it.code, text))
                        } catch (e: IOException) {
                            Reply.Failed(MacResult.Unreachable(e.message ?: e.javaClass.simpleName))
                        }
                    }
                    continuation.resume(reply)
                }
            })
        }

    companion object {
        const val POST_READ_TIMEOUT_SECONDS = 75L
    }
}
