package breez_sdk_liquid_notification.job

import android.content.Context
import breez_sdk_liquid.BindingLiquidSdk
import breez_sdk_liquid_notification.SdkForegroundService
import breez_sdk_liquid_notification.ServiceLogger
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json

@Serializable
data class NwcEventRequest(
  @SerialName("reply_url") val replyURL: String,
)

@Serializable
data class NwcEventResponse(
  val status: String = "OK",
  val message: String = "Event received",
)

class NwcEventJob(
  private val context: Context,
  private val fgService: SdkForegroundService,
  private val payload: String,
  private val logger: ServiceLogger,
) : Job {
  companion object {
    private const val TAG = "NwcEventJob"
  }

  override fun start(liquidSDK: BindingLiquidSdk) {
    var request: NwcEventRequest? = null
    try {
      val decoder = Json { ignoreUnknownKeys = true }
      request = decoder.decodeFromString(NwcEventRequest.serializer(), payload)

      val response = NwcEventResponse()
      val success = replyServer(Json.encodeToString(response), request.replyURL)
      logger.log(TAG, "Sent NWC ack (success=$success) to: ${request.replyURL}", "INFO")
    } catch (e: Exception) {
      logger.log(TAG, "Failed to process NWC event: ${e.message}", "WARN")
      request?.let {
        val errorResponse = mapOf("status" to "ERROR", "reason" to (e.message ?: "Unknown error"))
        try {
          replyServer(Json.encodeToString(errorResponse), it.replyURL)
        } catch (ignored: Exception) {
          logger.log(TAG, "Failed to send NWC error response: ${ignored.message}", "WARN")
        }
      }
    }

    fgService.onFinished(this)
  }

  override fun onShutdown() {}
}
