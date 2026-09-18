import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

// Bars of words per day for the last 30 days, oldest on the left.
Item {
    id: chart
    property var days: []
    implicitHeight: Kirigami.Units.gridUnit * 10

    readonly property int maxWords: {
        let m = 0;
        for (const d of days) m = Math.max(m, d.words);
        return m;
    }
    property int hovered: -1

    onDaysChanged: canvas.requestPaint()
    onHoveredChanged: canvas.requestPaint()
    onWidthChanged: canvas.requestPaint()
    Kirigami.Theme.inherit: true
    Connections {
        target: Kirigami.Theme
        function onColorsChanged() { canvas.requestPaint(); }
    }

    Canvas {
        id: canvas
        anchors.fill: parent
        anchors.bottomMargin: Kirigami.Units.gridUnit * 1.4
        antialiasing: true
        onPaint: {
            const ctx = getContext("2d");
            ctx.clearRect(0, 0, width, height);
            const n = chart.days.length;
            if (n === 0) return;
            const gap = 3;
            const bw = Math.max(2, (width - gap * (n - 1)) / n);
            const max = Math.max(1, chart.maxWords);
            // Baseline.
            ctx.strokeStyle = Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.25);
            ctx.lineWidth = 1;
            ctx.beginPath();
            ctx.moveTo(0, height - 0.5);
            ctx.lineTo(width, height - 0.5);
            ctx.stroke();
            for (let i = 0; i < n; i++) {
                const h = Math.max(chart.days[i].words > 0 ? 2 : 0, (height - 2) * chart.days[i].words / max);
                const x = i * (bw + gap);
                ctx.fillStyle = i === chart.hovered ? Kirigami.Theme.textColor
                              : (i === n - 1 ? Kirigami.Theme.highlightColor : Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.highlightColor, Kirigami.Theme.backgroundColor, 0.3));
                ctx.fillRect(x, height - 1 - h, bw, h);
            }
        }
        MouseArea {
            anchors.fill: parent
            hoverEnabled: true
            onPositionChanged: (mouse) => {
                const n = chart.days.length;
                if (n === 0) return;
                chart.hovered = Math.min(n - 1, Math.max(0, Math.floor(mouse.x / (width / n))));
            }
            onExited: chart.hovered = -1
        }
    }

    // Axis labels: first and last day, or the hovered day's value.
    QQC2.Label {
        anchors.left: parent.left
        anchors.bottom: parent.bottom
        text: chart.days.length > 0 ? chart.days[0].date : ""
        font: Kirigami.Theme.smallFont
        opacity: 0.7
    }
    QQC2.Label {
        anchors.horizontalCenter: parent.horizontalCenter
        anchors.bottom: parent.bottom
        text: chart.hovered >= 0 && chart.hovered < chart.days.length
            ? chart.days[chart.hovered].date + ": " + chart.days[chart.hovered].words + " words, " + chart.days[chart.hovered].utterances + " utterances"
            : "peak " + chart.maxWords + " words/day"
        font: Kirigami.Theme.smallFont
    }
    QQC2.Label {
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        text: chart.days.length > 0 ? "today" : ""
        font: Kirigami.Theme.smallFont
        opacity: 0.7
    }
}
