import Toybox.Application;
import Toybox.Communications;
import Toybox.Lang;
import Toybox.WatchUi;

// Everything goes through the Mnemonic app on the phone, which talks to the
// Mac over Tailscale, so the watch never needs a public address for the Mac.
class MnemonicApp extends Application.AppBase {
    var chatsMenu as ChatsMenu?;
    var chatView as ChatView?;
    var listeningView as ListeningView?;

    function initialize() {
        AppBase.initialize();
    }

    function onStart(state as Dictionary?) as Void {
        Communications.registerForPhoneAppMessages(method(:onPhone));
        send({"op" => "hello"});
    }

    function getInitialView() as [WatchUi.Views] or [WatchUi.Views, WatchUi.InputDelegates] {
        var menu = new ChatsMenu();
        chatsMenu = menu;
        return [menu, new ChatsMenuDelegate()];
    }

    function onPhone(message as Communications.PhoneAppMessage) as Void {
        var data = message.data;
        if (!(data instanceof Dictionary)) {
            return;
        }
        var type = text(data["t"]);
        var menu = chatsMenu;
        var chat = chatView;
        var listening = listeningView;
        if (type.equals("list")) {
            if (menu != null) {
                menu.show(data["chats"] as Array, text(data["error"]));
            }
        } else if (type.equals("heard")) {
            if (listening != null && listening.chatId.equals(text(data["id"]))) {
                listening.onHeard(text(data["text"]), text(data["error"]));
            }
        } else if (chat != null && chat.chatId.equals(text(data["id"]))) {
            chat.onMessage(type, data);
        }
        WatchUi.requestUpdate();
    }

    function send(message as Dictionary) as Void {
        Communications.transmit(message, null, new SendListener());
    }

    function onSendFailed() as Void {
        var chat = chatView;
        if (chat != null) {
            chat.status = "Can't reach the phone";
        } else {
            var menu = chatsMenu;
            if (menu != null) {
                menu.setTitle("No phone");
            }
        }
        WatchUi.requestUpdate();
    }
}

class SendListener extends Communications.ConnectionListener {
    function initialize() {
        ConnectionListener.initialize();
    }

    function onComplete() as Void {
    }

    function onError() as Void {
        mnemonic().onSendFailed();
    }
}

function mnemonic() as MnemonicApp {
    return Application.getApp() as MnemonicApp;
}

function text(value) as String {
    return value instanceof String ? value : "";
}
