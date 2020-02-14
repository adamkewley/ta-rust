// Returns game path extracted from URL GET params
// (e.g. <url>?param1=val)
function extractGamePathFromURLParams(parameterName) {
    var result = null,
        tmp = [];
    location.search
        .substr(1)
        .split("&")
        .forEach(function (item) {
            tmp = item.split("=");
            if (tmp[0] === parameterName) result = decodeURIComponent(tmp[1]);
        });
    return result;
}

// Returns a <span> element with the specified attribute set.
function createSpanWithAttr(attrName, attrValue) {
    const el = document.createElement("span");
    el.setAttribute(attrName, attrValue);
    return el;
}

// Abstraction for a console UI that slowly prints characters.
//
// I tried to make this as close as possible to typical bash CLIs that
// potentially than the user writes without making things too
// complicated. This always immediately prints whatever the user
// types.
class ConsoleUi {

    constructor(gameEl, outputEl, options) {
        const defaults = {
            msPerLetter: 1,
        };

        const opts = Object.assign({}, defaults, options);
        this.msPerLetter = opts.msPerLetter;

        this.gameEl = gameEl;
        this.outputEl = outputEl;

        // A queue of strings from the server that need to be printed
        // character-by-character
        //
        // [{ content: string }]
        this.queue = [];

        // Offset into the string at the head of the queue. Once this
        // is === queue[0].length, the queue head is popped (all chars
        // printed) and the counter is reset.
        //
        // number
        this.currentOffset = 0;

        // The current element that is being appended to
        // character-by-character. Switches whenever input alternates
        // from server to user.
        //
        // HTMLElement
        this.currentEl = createSpanWithAttr("from", "server");
        this.outputEl.appendChild(this.currentEl);

        // The handle returned by `window.setInterval`. Used to cancel
        // the interval event that updates the input
        // character-by-character
        //
        // window.setInterval::return_type
        this.intervalTicker = null;
    }

    start() {
        this.clearInterval();
        this.setInterval();
    }

    stop() {
        this.clearInterval();
    }

    setInterval() {
        this.intervalTicker = window.setInterval(() => {
            this.tick();
        }, this.msPerLetter);
    }

    clearInterval() {
        if (this.intervalTicker !== null) {
            window.clearInterval(this.intervalTicker);
            this.intervalTicker = null;
        }
    }

    tick() {
        if (this.queue.length === 0) {
            this.stop();
            return;
        }

        const head = this.queue[0];

        if (this.currentOffset === head.content.length) {
            // we're finished with the head of the queue
            this.queue.shift();
            this.currentOffset = 0;
            head.resolve();
            return;
        }

        this.currentEl.innerHTML += head.content[this.currentOffset++];
        window.scrollTo(0, document.body.scrollHeight);
    }

    // Appends output from the server to the UI. The content will
    // necessarily be written immediately. Returns a Promise that
    // resolves after the output appears in the UI.
    appendServer(content) {
        return new Promise((resolve, reject) => {
            this.queue.push({
                content: content,
                resolve: resolve,
            });

            // The ticker might've been cleared because the queue was
            // empty, so reset it to cause the tick loop to start churning
            // this new entry.
            if (this.intervalTicker === null) {
                this.setInterval();
            }
        });
    }

    appendClient(content) {
        let e;
        if (this.currentEl.hasAttribute("player")) {
            e = this.currentEl;
        } else {
            e = createSpanWithAttr("from", "player");
            this.outputEl.appendChild(e);
        }
        e.innerHTML += content;
        this.currentEl = createSpanWithAttr("from", "server");
        this.outputEl.appendChild(this.currentEl);
    }
}

// Bind the various HTML elements in the page into javascript
const gameAreaEl = document.getElementById("game-output");
const gameOutputEl = document.getElementById("text");
const userInputEl = document.getElementById("user-input");
const userFormEl = document.getElementById("user-form");
const userInputFeedbackEl = document.getElementById("user-input-feedback");

// Wrap relevant elements in a `ConsoleUi` abstraction that receives
// game text
const ui = new ConsoleUi(gameAreaEl, gameOutputEl);

// Build a path to the websocket that hosts a server-side console
// game.
const gamePath = extractGamePathFromURLParams("game");
const gameUrl = (window.location.protocol.match(/^https/) !== null) ?
          "wss://" + window.location.host + "/api" + gamePath :
          "ws://" + window.location.host + "/api" + gamePath;

// Opens a websocket to a server-side game and connects the websocket
// to the UI appropriately (prints text, takes user input, etc.)
const startGame = () => {
    const gameSocket = new WebSocket(gameUrl);

    const onWindowUnloadHandler = () => {
        // gameSocket.onclose = () => {};  // TODO: disable onclose first
        gameSocket.close();
    };

    const onBodyClickHandler = () => {
        userInputEl.focus();
    };

    const onInputHandler = (e) => {
        userInputFeedbackEl.innerHTML = userInputEl.value;
    };

    const onSubmitHandler = (e) => {
        ui.appendClient(userInputEl.value + "\n");
        gameSocket.send(userInputEl.value + "\n");
        userInputFeedbackEl.innerHTML = "";
        userInputEl.value = "";
        e.preventDefault();
    };

    gameSocket.onopen = () => {
        window.addEventListener("unload", onWindowUnloadHandler);
        document.body.addEventListener("click", onBodyClickHandler);
        userInputEl.addEventListener("input", onInputHandler);
        userFormEl.addEventListener("submit", onSubmitHandler);
    };

    gameSocket.onclose = function(e) {
        window.removeEventListener("unload", onWindowUnloadHandler);
        document.body.removeEventListener("click", onBodyClickHandler);
        userInputEl.removeEventListener("input", onInputHandler);
        userFormEl.removeEventListener("submit", onSubmitHandler);

        ui.appendServer("\n---GAME ENDED: PRESS ANY KEY OR CLICK TO RESTART---\n")
            .then(() => {
                userFormEl.addEventListener("submit", (e) => {
                    // For now, just reload the page whenever the user
                    // resubmits. This prevents having to bother to write a
                    // restarting state machine etc.
                    window.location.reload();
                    e.preventDefault();
                });

                window.onkeydown = function() {
                    window.location.reload();
                };
            });
    };

    gameSocket.onmessage = function(event) {
        const fr = new FileReader();
        fr.onload = function(t) {
            ui.appendServer(t.target.result);
        };
        fr.readAsText(event.data);
    };
};

// Immediately start the game on loading this script
startGame();
