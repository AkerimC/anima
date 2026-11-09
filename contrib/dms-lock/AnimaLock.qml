import QtQuick
import Quickshell

// Anima's characters along the bottom of the lock screen: animad keeps
// playing while locked and writes a frame to frame.png; this shows the
// newest one, two images taking turns so it never blanks while loading.
Item {
    id: root

    readonly property string anima: Quickshell.env("HOME") + "/Projeler/anima/target/release/anima"
    readonly property string frame: "file://" + Quickshell.env("XDG_RUNTIME_DIR") + "/anima-lock/frame.png"
    property int count: 0
    property int front: 0

    anchors.left: parent.left
    anchors.right: parent.right
    anchors.bottom: parent.bottom
    height: parent.height

    function tell(state) {
        Quickshell.execDetached([root.anima, "lockscreen", state]);
    }

    Component.onCompleted: tell("on")
    Component.onDestruction: tell("off")

    // animad lets go by itself if it stops hearing this
    Timer {
        interval: 4000
        running: true
        repeat: true
        onTriggered: root.tell("on")
    }

    Timer {
        interval: 50
        running: root.visible
        repeat: true
        onTriggered: {
            const back = root.front === 0 ? second : first;
            if (back.status === Image.Loading)
                return;
            root.count += 1;
            back.source = root.frame + "?" + root.count;
        }
    }

    Image {
        id: first
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        height: sourceSize.width > 0 ? width * sourceSize.height / sourceSize.width : 0
        cache: false
        asynchronous: true
        smooth: true
        visible: root.front === 0 && status === Image.Ready
        onStatusChanged: if (status === Image.Ready) root.front = 0
    }

    Image {
        id: second
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        height: sourceSize.width > 0 ? width * sourceSize.height / sourceSize.width : 0
        cache: false
        asynchronous: true
        smooth: true
        visible: root.front === 1 && status === Image.Ready
        onStatusChanged: if (status === Image.Ready) root.front = 1
    }
}
