# PR description template (PRD §6a item 1 — the primary reviewer surface).
# Versioned config: the meta-loop may propose changes to this file as PRs.
# Placeholders are substituted by the runner workflow via envsubst from the
# Work Order JSON and the measured diff footprint. A reviewer must be able
# to reach an approve/reject decision from the PR page alone.

${MERGE0_SUMMARY}

## Why this was judged safe to attempt

- **Success criterion (testable):** ${MERGE0_SUCCESS_CRITERIA}
- **Constraints honored:** ${MERGE0_CONSTRAINTS}
- **Suspect change:** ${MERGE0_SUSPECT_CHANGE}

## Evidence

${MERGE0_EVIDENCE}

## Repro

${MERGE0_REPRO}

## What the tests verify

`${MERGE0_TEST_COMMAND}` passed in this workflow run before the PR was
opened. No red PRs reach review.

## Diff footprint vs budget

${MERGE0_FILES_CHANGED} file(s), ${MERGE0_LINES_CHANGED} line(s) changed —
budget: ${MERGE0_BUDGET_FILES} files / ${MERGE0_BUDGET_LINES} lines.

---
Originating Merge0 report: `${MERGE0_REPORT_ID}` (see the inbox for gate
reasoning and full evidence).
