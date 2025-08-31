import UserNotifications
import Foundation

struct NwcEventRequest: Decodable {
  let event_id: String
}

class NwcEventTask : TaskProtocol {
  fileprivate let TAG = "NwcEventTask"
  
  private let pollingInterval: TimeInterval = 5.0
  
  private var request: NwcEventRequest?
  internal var payload: String
  internal var contentHandler: ((UNNotificationContent) -> Void)?
  internal var bestAttemptContent: UNMutableNotificationContent?
  internal var logger: ServiceLogger
  internal var notified: Bool = false
  private var pollingTimer: Timer?
  
  init(payload: String, logger: ServiceLogger, contentHandler: ((UNNotificationContent) -> Void)? = nil, bestAttemptContent: UNMutableNotificationContent? = nil) {
    self.payload = payload
    self.contentHandler = contentHandler
    self.bestAttemptContent = bestAttemptContent
    self.logger = logger
  }
  
  func start(liquidSDK: BindingLiquidSdk) throws {
    do {
      self.request = try JSONDecoder().decode(NwcEventRequest.self, from: self.payload.data(using: .utf8)!)
      self.logger.log(tag: TAG, line: "Starting SDK for NWC event with ID: \(request.event_id)", level: "INFO")
      startPolling(liquidSDK: liquidSDK)
    } catch let e {
      self.logger.log(tag: TAG, line: "failed to decode payload: \(e)", level: "ERROR")
      self.onShutdown()
      throw e
    }
  }
  
  private func startPolling(liquidSDK: BindingLiquidSdk) {
    pollingTimer = Timer.scheduledTimer(withTimeInterval: pollingInterval, repeats: true) { [weak self] _ in
      guard let self = self else { return }
      self.logger.log(tag: TAG, line: "Polling for NWC event with ID: \(self.request?.event_id ?? "unknown")", level: "TRACE")
    }
    
    pollingTimer?.fire()
  }
  
  private func stopPolling(withError error: Error? = nil) {
    pollingTimer?.invalidate()
    pollingTimer = nil
    
    if let error = error {
      logger.log(tag: TAG, line: "Polling stopped with error: \(error)", level: "ERROR")
      onShutdown()
    }
  }
  
  public func onEvent(e: SdkEvent) {
    if let eventId = self.request?.event_id {
      switch e {
      case .nwc(let nwcEvent, let eventIdFromEvent):
        if eventIdFromEvent == eventId {
          self.logger.log(tag: TAG, line: "Received matching NWC event with ID: \(eventId)", level: "INFO")
          self.notifySuccess(nwcEvent: nwcEvent)
          self.stopPolling()
        }
        break
      default:
        break
      }
    }
  }
  
  func onShutdown() {
    let notificationTitle = ResourceHelper.shared.getString(
      key: Constants.NWC_EVENT_NOTIFICATION_FAILURE_TITLE, 
      fallback: Constants.DEFAULT_NWC_EVENT_NOTIFICATION_FAILURE_TITLE
    )
    let notificationBody = ResourceHelper.shared.getString(
      key: Constants.NWC_EVENT_NOTIFICATION_FAILURE_TEXT, 
      fallback: Constants.DEFAULT_NWC_EVENT_NOTIFICATION_FAILURE_TEXT
    )
    self.displayPushNotification(title: notificationTitle, body: notificationBody, logger: self.logger, threadIdentifier: Constants.NOTIFICATION_THREAD_DISMISSIBLE)
  }

  private func notifySuccess(nwcEvent: NwcEvent) {
    if !self.notified {
      self.logger.log(tag: TAG, line: "NWC event processing successful for ID: \(self.request?.event_id ?? "unknown")", level: "INFO")
      self.notified = true
      
      let notificationTitle = ResourceHelper.shared.getString(
        key: Constants.NWC_EVENT_NOTIFICATION_TITLE,
        fallback: Constants.DEFAULT_NWC_EVENT_NOTIFICATION_TITLE
      )
      let notificationBody = ResourceHelper.shared.getString(
        key: Constants.NWC_EVENT_NOTIFICATION_TEXT,
        fallback: Constants.DEFAULT_NWC_EVENT_NOTIFICATION_TEXT
      )
      self.displayPushNotification(title: notificationTitle, body: notificationBody, logger: self.logger, threadIdentifier: Constants.NOTIFICATION_THREAD_DISMISSIBLE)
    }
  }
}
