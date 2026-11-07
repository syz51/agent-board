# Design 3: Columns

A warm, card-based board in the spirit of kanban tools: a slim icon rail, a stone canvas,
and white raised cards for boards, tasks and agents, with task groups laid out as columns.

## Key choices

- **Layout**: an 88px rail with each section as an icon over its label, the current one
  raised on a white tile; the product mark at the top and the connection at the foot. The
  page is wide (up to 1360px). Phones keep the bottom tabs.
- **Board as columns**: each status group is a tinted lane with its count, holding one
  card per task (status and title, then id, priority, owner and last update). Lanes sit
  side by side and wrap onto new rows; on a phone they stack.
- **Boards as cards**: a grid of board cards, three across on a wide screen: name and
  slug, goal, the task bar with counts in words, coordinator and asks, autopilot at the
  foot.
- **Task**: the description, acceptance, evidence, notes and history on one raised white
  sheet, the properties on a second card beside it.
- **Agents**: one card each, two across on a wide screen: who, state and where they run,
  then what they work on and their handoff.
- **Type and colour**: Inter at 14px; a warm stone canvas, white cards with a soft
  shadow, 12px corners, and one teal accent for links and the primary action.
- **Round-6 findings**: autopilot switches write On or Off; board goals open in full on
  click, tap or key; empty coordinators read "No coordinator"; event marks show the state
  an event leaves; agent cards keep the same order of facts.

## What it trades away

- Columns show a few tasks per screen per status; a long Ready lane needs scrolling,
  and tasks cannot be compared across statuses in one aligned list.
- Cards and shadows add visual weight; the page is friendlier but less dense.
- The rail drops the per-board list of the sidebar: boards are one click away on Boards.
