# Docket

Task tracking uses ranger: work corresponds to a task in the `docket`
backlog, and done means the change is committed. See the `ranger` skill
for the workflow.

`just` is the check suite: fmt, clippy, and coverage gated at 100%. Run
it before committing — `cargo test` alone doesn't meet the bar.
