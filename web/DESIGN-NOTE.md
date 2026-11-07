# Design 4: Console

A dense operator console in the spirit of terminal dashboards and admin tools: a dark
command bar, gridded tables with column heads and zebra rows, small type with mono ids,
graphite surfaces, blue links and amber kept for what needs attention.

## Key choices

- **Layout**: one dark command bar across the top with the product mark, the sections
  inline (the current one underlined in amber) and the connection state at the right.
  The page uses the full width. Phones keep the bottom tabs.
- **Board as a table**: one ruled table under a row of column heads (priority, status,
  id, title, owner, updated). Each status group is a shaded band with its count; rows
  are 32px with alternate shading. On a phone each row stacks into title, then id,
  priority, owner and time.
- **Boards as a table**: board, task bar with counts, coordinator, asks and autopilot in
  ruled columns, under the same kind of head row.
- **Task**: the description, acceptance, evidence, notes and history as ruled, striped
  lists; the properties as a key-value table beside them, with blockers and subtasks
  below.
- **Agents**: one table, a row per agent: who and their state, what they work on with
  the handoff, and where they run.
- **Events**: a striped log, one row per event, with time and sequence number at the
  right.
- **Asks**: square cards with a coloured left edge (amber for actions, violet for
  decisions); the single ask uses the full width.
- **Type and colour**: Inter at 12.5px with tabular figures, mono only for ids and
  code; 4px corners, no shadows; graphite neutrals and the primary action, blue links,
  and amber for the mark, the current section, the Needs you count and actions.
- **Round-6 findings**: autopilot switches write On or Off; board goals open in full on
  click, tap or key; empty coordinators read "No coordinator"; event marks show the state
  an event leaves; agent rows keep the same order of facts. The temporary-board note
  moves into the page subtitle, above the rule.

## What it trades away

- Small type and tight rows put more on screen but are harder to read for long, and
  less forgiving on a phone or with low vision settings (text still scales with zoom).
- Tables and rules everywhere look busy; there is little whitespace to rest the eye.
- The dark bar and amber accent read as a tool for operators rather than a friendly app.
- The per-board list of the sidebar is gone: boards are one click away on Boards.
