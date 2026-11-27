// The run form: a new container from an image, like `docker run -d`. Uses app.js's
// helpers. When the engine lacks the image, the Rust side pulls it first.
"use strict";

const RESTART_POLICIES = ["no", "on-failure", "always", "unless-stopped"];
let runBusy = false;

// A command line as words. Spaces split words; quotes and a backslash keep them.
// Null when a quote is not closed.
function splitCommand(text) {
  const words = [];
  let word = null;
  let quote = null;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (quote) {
      if (c === quote) quote = null;
      else if (c === "\\" && quote === "\"" && /["\\]/.test(text[i + 1] || "")) word += text[++i];
      else word += c;
    } else if (c === "'" || c === "\"") {
      quote = c;
      word ??= "";
    } else if (c === "\\" && i + 1 < text.length) {
      word = (word ?? "") + text[++i];
    } else if (/\s/.test(c)) {
      if (word !== null) words.push(word);
      word = null;
    } else {
      word = (word ?? "") + c;
    }
  }
  if (quote) return null;
  if (word !== null) words.push(word);
  return words;
}

const isHostPath = (s) => /^(\/|\\\\|[A-Za-z]:[\\/])/.test(s);
const PLAIN_NAME = /^[A-Za-z0-9][A-Za-z0-9_.-]*$/;

function portNumber(text) {
  if (!/^\d+$/.test(text)) return null;
  const n = Number(text);
  return n >= 1 && n <= 65535 ? n : null;
}

// One row of a list (ports, variables, volumes): its fields and a remove button.
function runRow(list, fields) {
  const row = h("div", { class: "run-row" }, fields,
    iconButton("close", t("run.removeRow"), () => {
      row.remove();
      list.parentElement.querySelector(".run-add").focus();
    }, "run-remove"));
  list.append(row);
  return row;
}

const runInput = (cls, label, placeholder, props) => h("input", {
  type: "text", class: cls, "aria-label": label, placeholder,
  autocomplete: "off", autocapitalize: "off", spellcheck: "false", ...props,
});

function portRow(list) {
  const protocol = h("select", { class: "run-protocol", "aria-label": t("run.protocol") },
    h("option", { value: "tcp", text: "TCP" }), h("option", { value: "udp", text: "UDP" }));
  return runRow(list, [
    runInput("run-host", t("run.hostPort"), "8080", { inputmode: "numeric" }),
    h("span", { class: "run-sep", "aria-hidden": "true", text: ":" }),
    runInput("run-container", t("run.containerPort"), "80", { inputmode: "numeric" }),
    protocol,
  ]);
}

function envRow(list) {
  return runRow(list, [runInput("run-env", t("run.env"), "KEY=VALUE")]);
}

function volumeRow(list) {
  return runRow(list, [
    runInput("run-source", t("run.volumeSource"), t("run.volumeSource")),
    h("span", { class: "run-sep", "aria-hidden": "true", text: "→" }),
    runInput("run-target", t("run.volumeTarget"), "/data"),
    h("label", { class: "check run-ro" }, h("input", { type: "checkbox", class: "run-readonly" }), h("span", { text: t("run.readOnly") })),
  ]);
}

// A titled list with an Add button; `add(list)` adds a row.
function runList(id, title, addLabel, add, hint) {
  const list = h("div", { class: "run-list", id, role: "group", "aria-labelledby": `${id}-title` });
  return h("div", { class: "run-section" },
    h("span", { class: "run-label", id: `${id}-title`, text: title }),
    list,
    hint ? h("p", { class: "hint", text: hint }) : null,
    h("button", { type: "button", class: "button small run-add", onclick: () => add(list).querySelector("input").focus() },
      h("span", { "aria-hidden": "true", text: "+" }), h("span", { text: addLabel })));
}

// The form's values as the Rust RunSpec, or the first problem: { field, message }.
function readRunForm(form) {
  const value = (el) => el.value.trim();
  const bad = (field, key, vars) => ({ field, message: t(key, vars) });
  const image = form.querySelector("#run-image");
  if (!value(image) || /\s/.test(value(image))) return bad(image, "run.error.image");
  const name = form.querySelector("#run-name");
  if (value(name) && !(PLAIN_NAME.test(value(name)) && value(name).length >= 2)) return bad(name, "run.error.name");
  const spec = { image: value(image), name: value(name), ports: [], env: [], volumes: [], command: [], restart: "no", autoRemove: false };
  const hosts = new Set();
  for (const row of form.querySelectorAll("#run-ports .run-row")) {
    const [host, container] = [row.querySelector(".run-host"), row.querySelector(".run-container")];
    if (!value(host) && !value(container)) continue;
    if (!value(container)) return bad(container, "run.error.containerPort");
    if (portNumber(value(container)) == null) return bad(container, "run.error.port");
    if (value(host) && portNumber(value(host)) == null) return bad(host, "run.error.port");
    const protocol = row.querySelector(".run-protocol").value;
    const hostPort = value(host) ? portNumber(value(host)) : 0;
    if (hostPort) {
      if (hosts.has(`${hostPort}/${protocol}`)) return bad(host, "run.error.portTwice", { port: `${hostPort}/${protocol}` });
      hosts.add(`${hostPort}/${protocol}`);
    }
    spec.ports.push({ host: hostPort, container: portNumber(value(container)), protocol });
  }
  for (const input of form.querySelectorAll("#run-env .run-env")) {
    if (!value(input)) continue;
    if (!/^[^=\s]+=/.test(value(input))) return bad(input, "run.error.env");
    spec.env.push(value(input));
  }
  for (const row of form.querySelectorAll("#run-volumes .run-row")) {
    const [source, target] = [row.querySelector(".run-source"), row.querySelector(".run-target")];
    if (!value(source) && !value(target)) continue;
    if (!isHostPath(value(source)) && !PLAIN_NAME.test(value(source))) return bad(source, "run.error.volumeSource");
    if (!value(target).startsWith("/") || value(target) === "/") return bad(target, "run.error.volumeTarget");
    spec.volumes.push({ source: value(source), target: value(target), readOnly: row.querySelector(".run-readonly").checked });
  }
  const command = form.querySelector("#run-command");
  const words = splitCommand(command.value);
  if (words == null) return bad(command, "run.error.command");
  spec.command = words;
  spec.autoRemove = form.querySelector("#run-rm").checked;
  spec.restart = spec.autoRemove ? "no" : form.querySelector("#run-restart").value;
  return { spec };
}

function openRunDialog(image = "") {
  if (runBusy || (S.engine && !S.engine.reachable)) return;
  document.getElementById("run-dialog")?.remove();
  const tags = [...new Set(S.lists.images.flatMap((i) => i.RepoTags || []).filter((x) => x !== "<none>:<none>"))];
  const status = h("p", { class: "progress run-status", id: "run-status", role: "status" });
  const restart = h("select", { id: "run-restart" },
    RESTART_POLICIES.map((p) => h("option", { value: p, text: t(`run.restart.${p}`) })));
  const autoRemove = h("input", { type: "checkbox", id: "run-rm", onchange: () => {
    restart.disabled = autoRemove.checked;
    if (autoRemove.checked) restart.value = "no";
  } });
  const field = (id, label, input, hint) => h("div", { class: "form-field" },
    h("label", { for: id, text: label }), input, hint ? h("p", { class: "hint", id: `${id}-hint`, text: hint }) : null);
  const text = (id, props) => h("input", { type: "text", id, autocomplete: "off", autocapitalize: "off", spellcheck: "false", ...props });
  const imageInput = text("run-image", { list: "run-images", placeholder: "nginx:alpine", value: image, "aria-required": "true" });
  const submit = h("button", { type: "submit", class: "button primary", value: "run" }, icon("start"), h("span", { text: t("action.run") }));
  const form = h("form", { method: "dialog", novalidate: true },
    h("h2", { id: "run-title", text: t("run.title") }),
    h("datalist", { id: "run-images" }, tags.map((tag) => h("option", { value: tag }))),
    h("div", { class: "run-grid" },
      field("run-image", t("run.image"), imageInput),
      field("run-name", t("run.name"), text("run-name", { placeholder: t("run.optional") }))),
    runList("run-ports", t("run.ports"), t("run.addPort"), portRow, t("run.portsHint")),
    h("details", { class: "run-more" },
      h("summary", {}, icon("forward"), h("span", { text: t("run.more") })),
      h("div", { class: "run-more-body" },
        runList("run-env", t("run.env"), t("run.addEnv"), envRow),
        runList("run-volumes", t("run.volumes"), t("run.addVolume"), volumeRow),
        field("run-command", t("run.command"), text("run-command", { class: "mono", placeholder: t("run.optional"), "aria-describedby": "run-command-hint" }), t("run.commandHint")),
        h("div", { class: "run-grid" },
          field("run-restart", t("run.restart"), restart),
          h("label", { class: "check run-rm" }, autoRemove, h("span", { text: t("run.autoRemove") }))))),
    status,
    h("div", { class: "buttons" },
      h("button", { type: "button", class: "button", text: t("action.cancel"), onclick: () => d.close() }),
      submit));
  const d = h("dialog", { class: "run", id: "run-dialog", "aria-labelledby": "run-title" }, form);
  portRow(form.querySelector("#run-ports"));

  const fields = () => form.querySelectorAll("input, select, button, textarea");
  // Enter submits from a list or a checkbox too, as it does from a text field.
  form.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.isComposing && (e.target.matches("select, input[type=checkbox]"))) {
      e.preventDefault();
      form.requestSubmit(submit);
    }
  });
  form.addEventListener("input", (e) => {
    if (e.target.getAttribute("aria-invalid")) {
      e.target.removeAttribute("aria-invalid");
      e.target.removeAttribute("aria-errormessage");
      status.textContent = "";
    }
  });
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    if (runBusy) return;
    form.querySelectorAll("[aria-invalid]").forEach((el) => { el.removeAttribute("aria-invalid"); el.removeAttribute("aria-errormessage"); });
    const read = readRunForm(form);
    status.className = "progress run-status";
    if (read.message) {
      status.className = "error-text run-status";
      status.textContent = read.message;
      read.field.setAttribute("aria-invalid", "true");
      read.field.setAttribute("aria-errormessage", "run-status");
      const more = read.field.closest("details");
      if (more) more.open = true;
      read.field.focus();
      return;
    }
    const spec = read.spec;
    runBusy = true;
    const enabled = [...fields()].filter((el) => !el.disabled);
    enabled.forEach((el) => { el.disabled = true; });
    status.textContent = t("run.starting", { name: spec.image });
    pullLine = (line) => { status.textContent = `${t("pull.pulling", { name: spec.image })}\n${line}`; };
    try {
      const id = await API.runContainer(spec);
      runBusy = false;
      d.close();
      await refresh();
      const c = findObject("containers", id);
      toast(t("done.start", { name: c ? containerName(c) : spec.name || shortId(id) }));
      if (c) openDetail("containers", c.Id, "logs");
    } catch (err) {
      status.className = "error-text run-status";
      status.textContent = String(err);
      enabled.forEach((el) => { el.disabled = false; });
      restart.disabled = autoRemove.checked;
    } finally {
      pullLine = null;
      runBusy = false;
    }
  });
  // Esc closes, but not while the container is being made.
  d.addEventListener("cancel", (e) => { if (runBusy) e.preventDefault(); });
  d.addEventListener("close", () => d.remove());
  document.body.append(d);
  d.showModal();
  (image ? form.querySelector("#run-name") : imageInput).focus();
  return d;
}
