---
triggers:
  - provider: github
    event: issue_created
    filter:
      repos:
        - <your-username>/termcraft
---
A new issue was opened. Run the triage stage for it.

- If the issue author is a bot or automation account, stop silently.
- If the issue was created carrying the `auto-implement` label, skip triage and go directly to the implementation stage (this label is a trusted creation-time shortcut).
- Otherwise dispatch triage, post the triage summary comment, and apply the recommended labels. Issues that arrive already carrying lifecycle labels (for example, imported or re-opened issues) still get a fresh triage pass.
- Stop after triage. Do not spec or implement; those stages wait for the human gate labels.
