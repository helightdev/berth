// The agent's machine inside Cockpit, two ways:
//   Desktop: the noVNC page that `tailscale serve` publishes on this host's
//            tailnet name, signed in when Cockpit has administrative access.
//   Console: a login shell as the agent, on a terminal of its own.
(function () {
    const note = document.getElementById("note");
    const open = document.getElementById("open");
    const desktopBox = document.getElementById("desktop");
    const consoleBox = document.getElementById("console");
    const t3Box = document.getElementById("t3");
    const buttons = {
        desktop: document.getElementById("show-desktop"),
        console: document.getElementById("show-console"),
        t3: document.getElementById("show-t3"),
    };
    const notes = { desktop: "", console: "", t3: "" };
    let shown = "desktop";
    let terminal = null;

    function say(view, text) {
        notes[view] = text;
        if (view === shown) note.textContent = text;
    }

    // The account the desktop belongs to, from the host's own record.
    const agent = cockpit.file("/etc/devbox/environment").read()
        .then(function (environment) {
            const user = /^(?:export )?DEVBOX_USER=(.*)$/m.exec(environment || "");
            return user ? user[1].replace(/["']/g, "").trim() : "agent";
        })
        .catch(function () { return "agent"; });

    function showDesktop() {
        cockpit.spawn(["tailscale", "status", "--json"], { err: "ignore" })
            .then(function (output) {
                const name = (JSON.parse(output).Self.DNSName || "").replace(/\.$/, "");
                if (!name) throw new Error("no name");
                return agent.then(function (user) {
                    return cockpit.file("/home/" + user + "/.config/tigervnc/password", { superuser: "try" }).read()
                        .catch(function () { return null; });
                }).then(function (password) {
                    let url = "https://" + name + "/vnc.html#autoconnect=1&resize=scale";
                    if (password) url += "&password=" + encodeURIComponent(password.trim());
                    // A new frame under a new name each time, with its address set
                    // before it joins the page: the browser then keeps no history
                    // entry for it and cannot restore it into another Cockpit page.
                    const frame = document.createElement("iframe");
                    frame.name = "devbox-desktop-" + Date.now();
                    frame.title = "Agent desktop";
                    frame.allow = "clipboard-read; clipboard-write; fullscreen";
                    frame.src = url;
                    desktopBox.replaceChildren(frame);
                    open.href = url;
                    open.hidden = false;
                    host = "https://" + name + ":8443";
                    say("desktop", "");
                });
            })
            .catch(function () {
                say("desktop", "The desktop is reached over the tailnet: enroll this host in Tailscale and publish it (berth: Publish desktop and T3 on the tailnet).");
            });
    }

    function startConsole() {
        agent.then(function (user) {
            terminal = new Terminal({ cursorBlink: true, fontSize: 14, scrollback: 5000 });
            const fit = new FitAddon.FitAddon();
            terminal.loadAddon(fit);
            terminal.open(consoleBox);
            fit.fit();
            const channel = cockpit.channel({
                payload: "stream",
                // agent-env adds what a plain switch of user lacks, such as the
                // session bus that systemctl --user and T3 need.
                spawn: ["runuser", "-l", user, "-c", 'exec agent-env "${SHELL:-/bin/bash}" -l'],
                pty: true,
                superuser: "require",
                environ: ["TERM=xterm-256color"],
                window: { rows: terminal.rows, cols: terminal.cols },
            });
            channel.addEventListener("message", function (event, data) { terminal.write(data); });
            channel.addEventListener("close", function (event, options) {
                say("console", options.problem === "access-denied"
                    ? "The console runs as " + user + ", which needs administrative access. Turn it on at the top of Cockpit, then choose Console again."
                    : "The shell has ended. Choose Console to start a new one.");
                terminal.dispose();
                terminal = null;
            });
            terminal.onData(function (data) { channel.send(data); });
            terminal.onResize(function (size) {
                channel.control({ window: { rows: size.rows, cols: size.cols } });
            });
            window.addEventListener("resize", function () { if (terminal && shown === "console") fit.fit(); });
            say("console", "");
            terminal.focus();
        });
    }

    // T3 asks every new browser or app to pair once. Its own pairing link points
    // at the host only, so the code is shown with the published tailnet address.
    let host = null;
    function pair() {
        const code = document.getElementById("t3-code");
        const browser = document.getElementById("t3-browser");
        const app = document.getElementById("t3-app");
        code.value = "";
        browser.hidden = app.hidden = true;
        say("t3", "");
        if (!host) {
            say("t3", "T3 is reached over the tailnet: enroll this host in Tailscale and publish it first.");
            return;
        }
        document.getElementById("t3-host").value = host;
        agent.then(function (user) {
            return cockpit.spawn(["runuser", "-l", user, "-c", "agent-env t3 pair --ttl 10m --label Cockpit"],
                                 { superuser: "require", err: "message" });
        }).then(function (output) {
            const token = /^Token:\s*(\S+)/m.exec(output);
            if (!token) throw new Error("T3 gave no pairing code.");
            const url = host + "/pair#token=" + encodeURIComponent(token[1]);
            code.value = token[1];
            browser.href = url;
            app.href = "t3code://pair?pairingUrl=" + encodeURIComponent(url);
            browser.hidden = app.hidden = false;
        }).catch(function (error) {
            say("t3", error.problem === "access-denied"
                ? "Pairing needs administrative access. Turn it on at the top of Cockpit, then choose New code."
                : "Could not get a pairing code: " + (error.message || error.problem || error));
        });
    }

    function copy(input, button) {
        const done = function () {
            button.textContent = "Copied";
            window.setTimeout(function () { button.textContent = "Copy"; }, 1500);
        };
        // The clipboard API is not always allowed inside Cockpit's frame.
        const fallback = function () {
            input.select();
            input.setSelectionRange(0, input.value.length);
            if (document.execCommand("copy")) done();
        };
        if (!input.value) return;
        if (navigator.clipboard && window.isSecureContext) {
            navigator.clipboard.writeText(input.value).then(done, fallback);
        } else {
            fallback();
        }
    }

    function show(view) {
        shown = view;
        desktopBox.hidden = view !== "desktop";
        consoleBox.hidden = view !== "console";
        t3Box.hidden = view !== "t3";
        for (const name in buttons) buttons[name].setAttribute("aria-pressed", String(view === name));
        note.textContent = notes[view];
        if (view === "console") {
            if (terminal) terminal.focus(); else startConsole();
        }
        if (view === "t3" && !document.getElementById("t3-code").value) pair();
    }

    buttons.desktop.addEventListener("click", function () { show("desktop"); });
    buttons.console.addEventListener("click", function () { show("console"); });
    buttons.t3.addEventListener("click", function () { show("t3"); });
    document.getElementById("t3-new").addEventListener("click", pair);
    t3Box.querySelectorAll("button[data-copy]").forEach(function (button) {
        button.addEventListener("click", function () {
            copy(document.getElementById(button.dataset.copy), button);
        });
    });
    showDesktop();
})();
