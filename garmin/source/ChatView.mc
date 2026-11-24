import Toybox.Graphics;
import Toybox.Lang;
import Toybox.WatchUi;

const BODY_FONT = Graphics.FONT_XTINY;
const ACCENT = 0xFF8A3D;

class ChatView extends WatchUi.View {
    var chatId as String;
    var title as String;
    var status as String = "Loading...";
    var call as String = "";
    var options as Array = [];
    var busy as Boolean = false;
    private var tool as String = "";
    private var body as String = "";
    private var lines as Array<String>?;
    private var top as Number = 0;
    private var visible as Number = 1;

    function initialize(id as String, label as String) {
        View.initialize();
        chatId = id;
        title = label;
    }

    function onMessage(type as String, data as Dictionary) as Void {
        if (type.equals("done")) {
            status = text(data["text"]);
            return;
        }
        if (!type.equals("chat")) {
            return;
        }
        title = text(data["title"]);
        var error = text(data["error"]);
        if (error.length() > 0) {
            status = error;
            return;
        }
        tool = text(data["tool"]);
        call = text(data["call"]);
        var offered = data["options"];
        options = offered instanceof Array ? offered : [];
        busy = data["busy"] == true;
        var reply = text(data["text"]);
        if (!reply.equals(body)) {
            top = 0;
        }
        body = reply;
        lines = null;
        status = busy ? "Working..." : "";
    }

    function scroll(pages as Number) as Void {
        var all = lines;
        if (all == null) {
            return;
        }
        var step = visible > 1 ? visible - 1 : 1;
        var last = all.size() - visible;
        top += pages * step;
        if (top > last) {
            top = last;
        }
        if (top < 0) {
            top = 0;
        }
        WatchUi.requestUpdate();
    }

    function onUpdate(dc as Graphics.Dc) as Void {
        var width = dc.getWidth();
        var height = dc.getHeight();
        dc.setColor(Graphics.COLOR_WHITE, Graphics.COLOR_BLACK);
        dc.clear();

        dc.setColor(Graphics.COLOR_LT_GRAY, Graphics.COLOR_TRANSPARENT);
        dc.drawText(width / 2, height * 8 / 100, Graphics.FONT_XTINY, fitted(dc, title, width * 60 / 100), Graphics.TEXT_JUSTIFY_CENTER);

        var bodyTop = height * 21 / 100;
        var bodyBottom = height * 80 / 100;
        var bodyWidth = width * 80 / 100;
        var all = lines;
        if (all == null) {
            all = [] as Array<String>;
            if (tool.length() > 0) {
                all.addAll(wrap(dc, "Wants to: " + tool, bodyWidth));
                all.add("");
            }
            all.addAll(wrap(dc, body.length() > 0 ? body : "No reply yet", bodyWidth));
            lines = all;
        }
        var lineHeight = dc.getFontHeight(BODY_FONT);
        visible = (bodyBottom - bodyTop) / lineHeight;
        var toolLines = tool.length() > 0 ? wrap(dc, "Wants to: " + tool, bodyWidth).size() : 0;
        for (var row = 0; row < visible && top + row < all.size(); row += 1) {
            var index = top + row;
            dc.setColor(index < toolLines ? ACCENT : Graphics.COLOR_WHITE, Graphics.COLOR_TRANSPARENT);
            dc.drawText((width - bodyWidth) / 2, bodyTop + row * lineHeight, BODY_FONT, all[index], Graphics.TEXT_JUSTIFY_LEFT);
        }

        var footer = status;
        if (footer.length() == 0) {
            footer = top + visible < all.size() ? "DOWN for more" : "START to reply";
        }
        dc.setColor(status.length() > 0 ? ACCENT : Graphics.COLOR_DK_GRAY, Graphics.COLOR_TRANSPARENT);
        dc.drawText(width / 2, height * 84 / 100, Graphics.FONT_XTINY, fitted(dc, footer, width * 64 / 100), Graphics.TEXT_JUSTIFY_CENTER);
    }
}

class ChatDelegate extends WatchUi.BehaviorDelegate {
    private var view as ChatView;

    function initialize(chat as ChatView) {
        BehaviorDelegate.initialize();
        view = chat;
    }

    function onSelect() as Boolean {
        WatchUi.pushView(new ActionsMenu(view), new ActionsDelegate(view), WatchUi.SLIDE_UP);
        return true;
    }

    function onMenu() as Boolean {
        return onSelect();
    }

    function onNextPage() as Boolean {
        view.scroll(1);
        return true;
    }

    function onPreviousPage() as Boolean {
        view.scroll(-1);
        return true;
    }

    function onBack() as Boolean {
        var app = mnemonic();
        app.chatView = null;
        app.send({"op" => "close"});
        WatchUi.popView(WatchUi.SLIDE_RIGHT);
        return true;
    }
}

const QUICK_REPLIES = ["Continue", "Yes, go ahead", "Open a PR"];

class ActionsMenu extends WatchUi.Menu2 {
    function initialize(view as ChatView) {
        Menu2.initialize({:title => "Reply"});
        if (view.call.length() > 0) {
            for (var index = 0; index < view.options.size(); index += 1) {
                var option = view.options[index] as Dictionary;
                addItem(new WatchUi.MenuItem(text(option["name"]), null, {"op" => "answer", "option" => text(option["id"])}, null));
            }
        }
        addItem(new WatchUi.MenuItem("Speak", "Records on the phone", {"op" => "listen"}, null));
        for (var index = 0; index < QUICK_REPLIES.size(); index += 1) {
            addItem(new WatchUi.MenuItem(QUICK_REPLIES[index], null, {"op" => "send", "text" => QUICK_REPLIES[index]}, null));
        }
        if (view.busy) {
            addItem(new WatchUi.MenuItem("Stop", null, {"op" => "stop"}, null));
        }
    }
}

class ActionsDelegate extends WatchUi.Menu2InputDelegate {
    private var view as ChatView;

    function initialize(chat as ChatView) {
        Menu2InputDelegate.initialize();
        view = chat;
    }

    function onSelect(item as WatchUi.MenuItem) as Void {
        var action = item.getId() as Dictionary;
        var op = text(action["op"]);
        var app = mnemonic();
        if (op.equals("listen")) {
            var listening = new ListeningView(view.chatId);
            app.listeningView = listening;
            app.send({"op" => "listen", "id" => view.chatId});
            WatchUi.switchToView(listening, new ListeningDelegate(listening, view), WatchUi.SLIDE_LEFT);
            return;
        }
        var message = {"op" => op, "id" => view.chatId};
        if (op.equals("answer")) {
            message["call"] = view.call;
            message["option"] = action["option"];
        } else if (op.equals("send")) {
            message["text"] = action["text"];
        }
        app.send(message);
        view.status = "Sending...";
        WatchUi.popView(WatchUi.SLIDE_DOWN);
    }
}

// The phone records until START, so pauses don't cut it short, and the
// Mac transcribes it.
class ListeningView extends WatchUi.View {
    var chatId as String;
    var heard as String = "";
    var error as String = "";
    var transcribing as Boolean = false;

    function initialize(id as String) {
        View.initialize();
        chatId = id;
    }

    function onHeard(words as String, problem as String) as Void {
        heard = words;
        error = problem;
        transcribing = false;
    }

    function isRecording() as Boolean {
        return heard.length() == 0 && error.length() == 0 && !transcribing;
    }

    function finish() as Void {
        transcribing = true;
        mnemonic().send({"op" => "finish", "id" => chatId});
        WatchUi.requestUpdate();
    }

    function listenAgain() as Void {
        heard = "";
        error = "";
        mnemonic().send({"op" => "listen", "id" => chatId});
        WatchUi.requestUpdate();
    }

    function onUpdate(dc as Graphics.Dc) as Void {
        var width = dc.getWidth();
        var height = dc.getHeight();
        dc.setColor(Graphics.COLOR_WHITE, Graphics.COLOR_BLACK);
        dc.clear();
        var lines;
        var hint;
        if (heard.length() > 0) {
            lines = wrap(dc, heard, width * 80 / 100);
            hint = "START send, BACK cancel";
        } else if (error.length() > 0) {
            lines = wrap(dc, error, width * 80 / 100);
            hint = "START to try again";
        } else if (transcribing) {
            lines = ["Transcribing..."];
            hint = "BACK cancel";
        } else {
            lines = ["Recording on", "the phone"];
            hint = "START when done";
        }
        var lineHeight = dc.getFontHeight(BODY_FONT);
        var shown = lines.size() < 6 ? lines.size() : 6;
        var y = height / 2 - shown * lineHeight / 2;
        for (var index = 0; index < shown; index += 1) {
            dc.drawText(width / 2, y + index * lineHeight, BODY_FONT, lines[index], Graphics.TEXT_JUSTIFY_CENTER);
        }
        dc.setColor(ACCENT, Graphics.COLOR_TRANSPARENT);
        dc.drawText(width / 2, height * 84 / 100, Graphics.FONT_XTINY, hint, Graphics.TEXT_JUSTIFY_CENTER);
    }
}

class ListeningDelegate extends WatchUi.BehaviorDelegate {
    private var view as ListeningView;
    private var chat as ChatView;

    function initialize(listening as ListeningView, chatView as ChatView) {
        BehaviorDelegate.initialize();
        view = listening;
        chat = chatView;
    }

    function onSelect() as Boolean {
        if (view.heard.length() > 0) {
            mnemonic().send({"op" => "send", "id" => view.chatId, "text" => view.heard});
            chat.status = "Sending...";
            close();
        } else if (view.error.length() > 0) {
            view.listenAgain();
        } else if (view.isRecording()) {
            view.finish();
        }
        return true;
    }

    function onBack() as Boolean {
        if (view.isRecording()) {
            mnemonic().send({"op" => "cancel"});
        }
        close();
        return true;
    }

    private function close() as Void {
        mnemonic().listeningView = null;
        WatchUi.popView(WatchUi.SLIDE_RIGHT);
    }
}

function fitted(dc as Graphics.Dc, value as String, width as Number) as String {
    if (dc.getTextWidthInPixels(value, Graphics.FONT_XTINY) <= width) {
        return value;
    }
    var cut = value;
    while (cut.length() > 1 && dc.getTextWidthInPixels(cut + "...", Graphics.FONT_XTINY) > width) {
        cut = cut.substring(0, cut.length() - 1) as String;
    }
    return cut + "...";
}

function wrap(dc as Graphics.Dc, value as String, width as Number) as Array<String> {
    var result = [] as Array<String>;
    var paragraphs = split(value, '\n');
    for (var index = 0; index < paragraphs.size(); index += 1) {
        var words = split(paragraphs[index], ' ');
        var line = "";
        for (var word = 0; word < words.size(); word += 1) {
            var next = words[word];
            var joined = line.length() > 0 ? line + " " + next : next;
            if (dc.getTextWidthInPixels(joined, BODY_FONT) <= width) {
                line = joined;
                continue;
            }
            if (line.length() > 0) {
                result.add(line);
            }
            line = next;
            // A word wider than the screen, like a path, is broken anywhere.
            while (line.length() > 1 && dc.getTextWidthInPixels(line, BODY_FONT) > width) {
                var cut = line.length() - 1;
                while (cut > 1 && dc.getTextWidthInPixels(line.substring(0, cut) as String, BODY_FONT) > width) {
                    cut -= 1;
                }
                result.add(line.substring(0, cut) as String);
                line = line.substring(cut, line.length()) as String;
            }
        }
        result.add(line);
    }
    return result;
}

function split(value as String, separator as Char) as Array<String> {
    var parts = [] as Array<String>;
    var chars = value.toCharArray();
    var start = 0;
    for (var index = 0; index < chars.size(); index += 1) {
        if (chars[index] == separator) {
            parts.add(value.substring(start, index) as String);
            start = index + 1;
        }
    }
    parts.add(value.substring(start, chars.size()) as String);
    return parts;
}
