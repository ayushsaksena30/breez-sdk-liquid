import UserNotifications
import Foundation

struct NwcEventRequest: Codable {
  let reply_url: String
  let event_id: String
}

struct NwcEventResponse: Decodable, Encodable {
  let status: String
  let message: String
  
  init() {
    self.status = "OK"
    self.message = "Event received"
  }
}

class NwcEventTask : ReplyableTask {
  fileprivate let TAG = "NwcEventTask"
  private let EVENT_WAIT_TIMEOUT_SEC: TimeInterval = 15.0
  
  private var request: NwcEventRequest?
  private var eventReceived = false
  private var responseSent = false
  private var timeoutTimer: Timer?
  
  init(payload: String, logger: ServiceLogger, contentHandler: ((UNNotificationContent) -> Void)? = nil, bestAttemptContent: UNMutableNotificationContent? = nil) {
    let successNotificationTitle = ResourceHelper.shared.getString(key: Constants.NWC_EVENT_NOTIFICATION_TITLE, fallback: Constants.DEFAULT_NWC_EVENT_NOTIFICATION_TITLE)
    let failNotificationTitle = ResourceHelper.shared.getString(key: Constants.NWC_EVENT_NOTIFICATION_FAILURE_TITLE, fallback: Constants.DEFAULT_NWC_EVENT_NOTIFICATION_FAILURE_TITLE)
    super.init(payload: payload, logger: logger, contentHandler: contentHandler, bestAttemptContent: bestAttemptContent, successNotificationTitle: successNotificationTitle, failNotificationTitle: failNotificationTitle)
  }
  
  override func start(liquidSDK: BindingLiquidSdk) throws {
    do {
      request = try JSONDecoder().decode(NwcEventRequest.self, from: self.payload.data(using: .utf8)!)
      
      self.logger.log(tag: TAG, line: "Waiting for NWC event with ID: \(request!.event_id)", level: "INFO")
      
      timeoutTimer = Timer.scheduledTimer(withTimeInterval: EVENT_WAIT_TIMEOUT_SEC, repeats: false) { [weak self] _ in
        guard let self = self else { return }
        if !self.eventReceived {
          self.logger.log(tag: self.TAG, line: "Timeout waiting for NWC event with ID: \(self.request!.event_id)", level: "WARN")
          self.sendErrorResponse(reason: "Timeout waiting for NWC event")
          self.displayPushNotification(title: self.failNotificationTitle, logger: self.logger, threadIdentifier: Constants.NOTIFICATION_THREAD_REPLACEABLE)
        }
      }
      
    } catch let e {
      self.logger.log(tag: TAG, line: "failed to decode payload: \(e)", level: "ERROR")
      sendErrorResponse(reason: e.localizedDescription)
      throw e
    }
  }
  
  override func onEvent(e: SdkEvent) {
    if case .nwc = e, let request = request {
      self.logger.log(tag: TAG, line: "Received NWC event, SDK is awake", level: "INFO")
      
      if !eventReceived {
        eventReceived = true
        
        let response = NwcEventResponse()
        let success = self.replyServer(encodable: response, replyURL: request.reply_url)
        self.logger.log(tag: TAG, line: "Sent NWC ack (success=\(success)) to: \(request.reply_url)", level: "INFO")
        
        responseSent = true
        timeoutTimer?.invalidate()
        timeoutTimer = nil
      }
    }
  }
  
  private func sendErrorResponse(reason: String) {
    guard let request = request, !responseSent else { return }
    
    let errorResponse = ["status": "ERROR", "reason": reason]
    do {
      let errorData = try JSONSerialization.data(withJSONObject: errorResponse)
      self.replyServer(data: errorData, replyURL: request.reply_url)
      self.logger.log(tag: TAG, line: "Sent error response: \(reason)", level: "WARN")
    } catch {
      self.logger.log(tag: TAG, line: "failed to send NWC error response: \(error)", level: "WARN")
    }
    
    responseSent = true
  }
  
  deinit {
    timeoutTimer?.invalidate()
    if !responseSent {
      sendErrorResponse(reason: "Task was deallocated")
    }
  }
}
