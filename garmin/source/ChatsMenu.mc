import Toybox.Lang;
import Toybox.WatchUi;

class ChatsMenu extends WatchUi.Menu2 {
    private var count as Number = 0;

    function initialize() {
        Menu2.initialize({:title => "Mnemonic"});
        show([], "Waiting for the phone");
    }

    function show(rows as Array, error as String) as Void {
        while (count > 0) {
            deleteItem(0);
            count -= 1;
        }
        setTitle(error.length() > 0 && rows.size() > 0 ? "Offline" : "Mnemonic");
        if (rows.size() == 0) {
            // Selecting this asks the phone again.
            addItem(new WatchUi.MenuItem(error.length() > 0 ? "Not linked" : "All clear", error.length() > 0 ? error : "Nothing needs you", :retry, null));
            count = 1;
            return;
        }
        for (var index = 0; index < rows.size(); index += 1) {
            var row = rows[index] as Dictionary;
            addItem(new WatchUi.MenuItem(text(row["title"]), text(row["status"]), text(row["id"]), null));
        }
        count = rows.size();
    }
}

class ChatsMenuDelegate extends WatchUi.Menu2InputDelegate {
    function initialize() {
        Menu2InputDelegate.initialize();
    }

    function onSelect(item as WatchUi.MenuItem) as Void {
        var id = item.getId();
        var app = mnemonic();
        if (!(id instanceof String)) {
            app.send({"op" => "hello"});
            return;
        }
        var view = new ChatView(id, item.getLabel());
        app.chatView = view;
        app.send({"op" => "open", "id" => id});
        WatchUi.pushView(view, new ChatDelegate(view), WatchUi.SLIDE_LEFT);
    }
}
