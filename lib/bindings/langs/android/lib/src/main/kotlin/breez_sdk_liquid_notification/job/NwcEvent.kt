package breez_sdk_liquid_notification.job

import android.content.Context
import android.os.Handler
import android.os.Looper
import breez_sdk_liquid.BindingLiquidSdk
import breez_sdk_liquid.SdkEvent
import breez_sdk_liquid_notification.SdkForegroundService
import breez_sdk_liquid_notification.ServiceLogger
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json

@Serializable
data class NwcEventRequest(
  @SerialName("reply_url") val replyURL: String,
  @SerialName("event_id") val eventId: String,
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
    private const val EVENT_WAIT_TIMEOUT_MS = 15_000L
  }

  private var request: NwcEventRequest? = null
  private var eventReceived = false
  private var responseSent = false
  private val timeoutHandler = Handler(Looper.getMainLooper())
  private val timeoutRunnable = Runnable {
    if (!eventReceived) {
      logger.log(TAG, "Timeout waiting for NWC event with ID: ${request?.eventId}", "WARN")
      sendErrorResponse("Timeout waiting for NWC event")
      fgService.onFinished(this)
    }
  }

  override fun start(liquidSDK: BindingLiquidSdk) {
    try {
      val decoder = Json { ignoreUnknownKeys = true }
      request = decoder.decodeFromString(NwcEventRequest.serializer(), payload)
      
      logger.log(TAG, "Waiting for NWC event with ID: ${request!!.eventId}", "INFO")
      
      timeoutHandler.postDelayed(timeoutRunnable, EVENT_WAIT_TIMEOUT_MS)
      
    } catch (e: Exception) {
      logger.log(TAG, "Failed to decode NWC event payload: ${e.message}", "ERROR")
      sendErrorResponse(e.message ?: "Unknown error")
      fgService.onFinished(this)
    }
  }

  override fun onEvent(e: SdkEvent) {
    if (e is SdkEvent.NWC && !eventReceived && request != null) {
      logger.log(TAG, "Received NWC SDK event, checking if it matches our event_id", "DEBUG")
      
      eventReceived = true
      timeoutHandler.removeCallbacks(timeoutRunnable)
      
      val response = NwcEventResponse()
      val success = replyServer(Json.encodeToString(response), request.replyURL)
      logger.log(TAG, "Sent NWC ack (success=$success) to: ${request.replyURL}", "INFO")
      
      responseSent = true
      fgService.onFinished(this)
    }
  }

  private fun sendErrorResponse(reason: String) {
    if (responseSent) return
    
    request?.let {
      val errorResponse = mapOf("status" to "ERROR", "reason" to reason)
      try {
        val success = replyServer(Json.encodeToString(errorResponse), it.replyURL)
        if (success) {
          logger.log(TAG, "Sent error response: $reason", "WARN")
        } else {
          logger.log(TAG, "Failed to send error response", "ERROR")
        }
      } catch (e: Exception) {
        logger.log(TAG, "Failed to send error response: ${e.message}", "ERROR")
      }
    }
    
    responseSent = true
  }

  override fun onShutdown() {
    timeoutHandler.removeCallbacks(timeoutRunnable)
    if (!responseSent) {
      sendErrorResponse("Job was shut down")
    }
  }
}
