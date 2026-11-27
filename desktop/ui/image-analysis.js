// The image detail tab: scan only when its button is pressed.
"use strict";

const ANALYSIS_SEVERITIES = ["critical", "high", "medium", "low", "unknown"];
const ANALYSIS_PAGE = 50;

function imageAnalysisPanel(detail, image) {
  const panel = h("div", { class: "panel image-analysis" });
  const state = detail.analysis || (detail.analysis = { report: null, error: null, running: false });

  async function analyze() {
    if (state.running) return;
    state.running = true;
    state.error = null;
    state.report = null;
    render();
    try {
      const report = await API.analyzeImage(image.Id);
      if (report.imageId !== image.Id) throw "imageUnavailable";
      state.report = report;
    } catch (error) {
      state.error = ["busy", "engineChanged", "imageUnavailable", "scannerMissing", "export", "workspace", "scanner", "report"]
        .includes(error) ? error : "scanner";
    } finally {
      state.running = false;
      if (S.detail === detail && detail.tab === "analysis") {
        if (panel.isConnected) render();
        else renderMain(true);
      }
    }
  }

  function render() {
    const scan = button("search", t(state.running ? "analysis.scanning" : "analysis.scan"), analyze, "primary");
    scan.disabled = state.running;
    put(panel,
      h("div", { class: "analysis-actions" },
        h("p", { class: "hint", text: t("analysis.hint") }), scan),
      state.running ? h("p", { role: "status", class: "visually-hidden", text: t("analysis.scanning") }) : null,
      state.error ? h("p", { role: "alert", class: "error-text analysis-error", text: t(`analysis.error.${state.error}`) }) : null,
      state.report ? reportPanel(state.report) : null);
  }

  render();
  return panel;
}

function reportPanel(report) {
  const number = new Intl.NumberFormat(I18N.lang);
  const counts = new Map(ANALYSIS_SEVERITIES.map((severity) => [severity, 0]));
  for (const finding of report.findings) counts.set(finding.severity, (counts.get(finding.severity) || 0) + 1);
  return h("div", { class: "analysis-report" },
    h("section", { "aria-labelledby": "analysis-findings-title" },
      h("h2", { id: "analysis-findings-title", class: "panel-title" }, t("analysis.findings", { n: number.format(report.findings.length) })),
      h("dl", { class: "analysis-summary" }, ANALYSIS_SEVERITIES.map((severity) =>
        h("div", { class: `analysis-level ${severity}` },
          h("dt", { text: t(`analysis.level.${severity}`) }),
          h("dd", { text: number.format(counts.get(severity)) })))),
      report.findings.length ? analysisTable(
        ["analysis.severity", "analysis.advisory", "analysis.package", "analysis.installed", "analysis.fixed", "analysis.target"],
        report.findings, (finding) => [
          h("span", { class: `analysis-badge ${finding.severity}`, text: t(`analysis.level.${finding.severity}`) }),
          h("span", { class: "mono", text: finding.advisory }),
          finding.package, finding.installed, finding.fixed || t("detail.none"), finding.target,
        ], "analysis-findings-title") :
        h("p", { class: "hint", text: t("analysis.noFindings") })),
    h("section", { "aria-labelledby": "analysis-packages-title" },
      h("h2", { id: "analysis-packages-title", class: "panel-title" }, t("analysis.packages", { n: number.format(report.packages.length) })),
      report.packages.length ? analysisTable(
        ["analysis.package", "settings.version", "analysis.target"],
        report.packages, (pkg) => [pkg.name, pkg.version || t("detail.none"), pkg.target], "analysis-packages-title") :
        h("p", { class: "hint", text: t("analysis.noPackages") })));
}

function analysisTable(columns, rows, cells, label) {
  let shown = 0;
  const more = button(null, t("analysis.more"), showNext);
  const body = h("tbody", {});
  const table = h("table", { "aria-labelledby": label },
    h("thead", {}, h("tr", {}, columns.map((key) => h("th", { scope: "col", text: t(key) })))), body);
  function showNext() {
    const next = rows.slice(shown, shown + ANALYSIS_PAGE);
    body.append(...next.map((row) => h("tr", {}, cells(row).map((cell) => h("td", {}, cell)))));
    shown += next.length;
    more.hidden = shown >= rows.length;
  }
  showNext();
  return h("div", { class: "analysis-results" }, h("div", { class: "analysis-table" }, table), more);
}
