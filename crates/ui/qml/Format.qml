import QtQuick

// Formatting helpers shared by the pages.
QtObject {
    function timeAgo(atMs) {
        const s = Math.max(0, (Date.now() - atMs) / 1000);
        if (s < 45) return "just now";
        if (s < 90) return "a minute ago";
        const m = s / 60;
        if (m < 60) return Math.round(m) + " min ago";
        const h = m / 60;
        if (h < 36) return Math.round(h) + (Math.round(h) === 1 ? " hour ago" : " hours ago");
        const d = h / 24;
        if (d < 14) return Math.round(d) + " days ago";
        return new Date(atMs).toLocaleDateString(Qt.locale(), Locale.ShortFormat);
    }

    function when(atMs) {
        const d = new Date(atMs);
        const today = new Date();
        const sameDay = d.toDateString() === today.toDateString();
        return sameDay ? d.toLocaleTimeString(Qt.locale(), Locale.ShortFormat)
                       : d.toLocaleString(Qt.locale(), Locale.ShortFormat);
    }

    function outcomeIcon(outcome) {
        switch (outcome) {
        case "typed": return "input-keyboard";
        case "snippet": return "edit-paste";
        case "raw": return "input-keyboard";
        case "command": return "system-run";
        case "confirm": return "dialog-question";
        case "refused": return "dialog-cancel";
        case "error": return "dialog-error";
        default: return "dialog-information";
        }
    }

    function outcomeLabel(record) {
        switch (record.outcome) {
        case "typed": return "typed";
        case "snippet": return "snippet";
        case "raw": return "typed as heard";
        case "command": return record.detail !== "" ? record.detail : "command";
        case "confirm": return "confirmed";
        case "refused": return record.detail !== "" ? "refused: " + record.detail : "refused";
        case "error": return record.detail !== "" ? "failed: " + record.detail : "failed";
        default: return record.outcome;
        }
    }

    function appName(cls) {
        if (!cls) return "unknown app";
        const parts = cls.split(".");
        return parts[parts.length - 1];
    }

    function number(n) {
        return Number(n).toLocaleString(Qt.locale(), "f", 0);
    }
}
