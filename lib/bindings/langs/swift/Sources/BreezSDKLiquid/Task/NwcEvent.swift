import UserNotifications
import Foundation

struct NwcEventRequest: Codable {
  let reply_url: String
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
  
  init(payload: String, logger: ServiceLogger, contentHandler: ((UNNotificationContent) -> Void)? = nil, bestAttemptContent: UNMutableNotificationContent? = nil) {
    let successNotificationTitle = ResourceHelper.shared.getString(key: Constants.NWC_EVENT_NOTIFICATION_TITLE, fallback: Constants.DEFAULT_NWC_EVENT_NOTIFICATION_TITLE)
    let failNotificationTitle = ResourceHelper.shared.getString(key: Constants.NWC_EVENT_NOTIFICATION_FAILURE_TITLE, fallback: Constants.DEFAULT_NWC_EVENT_NOTIFICATION_FAILURE_TITLE)
    super.init(payload: payload, logger: logger, contentHandler: contentHandler, bestAttemptContent: bestAttemptContent, successNotificationTitle: successNotificationTitle, failNotificationTitle: failNotificationTitle)
  }
  
  override func start(liquidSDK: BindingLiquidSdk) throws {
    var request: NwcEventRequest? = nil
    do {
      request = try JSONDecoder().decode(NwcEventRequest.self, from: self.payload.data(using: .utf8)!)
    } catch let e {
      self.logger.log(tag: TAG, line: "failed to decode payload: \(e)", level: "ERROR")
      self.displayPushNotification(title: self.failNotificationTitle, logger: self.logger, threadIdentifier: Constants.NOTIFICATION_THREAD_REPLACEABLE)
      throw e
    }
    
    do {
      let response = NwcEventResponse()
      self.replyServer(encodable: response, replyURL: request!.reply_url)
      
    } catch let e {
      self.logger.log(tag: TAG, line: "failed to process NWC event: \(e)", level: "ERROR")
      if let request = request {
        let errorResponse = ["status": "ERROR", "reason": e.localizedDescription]
        do {
          let errorData = try JSONSerialization.data(withJSONObject: errorResponse)
          self.replyServer(data: errorData, replyURL: request.reply_url)
        } catch {
          self.logger.log(tag: TAG, line: "failed to send NWC error response: \(error)", level: "WARN")
        }
      }
      self.displayPushNotification(title: self.failNotificationTitle, logger: self.logger, threadIdentifier: Constants.NOTIFICATION_THREAD_REPLACEABLE)
    }
  }
}
