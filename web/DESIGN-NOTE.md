# Design 5: Document

A calm, editorial take: every page reads like a well-set document in one centred column,
with serif headings, hairline rules instead of boxes, and ink-blue as the only accent.

## Key choices

- **Layout**: one centred column, 880px wide, under a quiet top bar: the product name on
  the left, the sections as a segmented pill in the middle, the connection on the right.
  There is no sidebar. Phones keep the bottom tabs.
- **No boxes**: lists, tables and sections are divided by hairline rules; each section
  heading sits on a darker rule. Only inputs, buttons and badges keep a border.
- **Board**: each status group is a section with a serif heading and its count, its tasks
  in ruled rows (priority, status, id, title, owner, updated). Groups follow one another
  down the page.
- **Boards**: ruled rows under a row of column labels: the board name in serif with its
  slug and goal, the task bar with counts, coordinator, asks and autopilot.
- **Task**: the title large in serif; description, acceptance, evidence, notes and history
  as ruled sections in the main column, the properties in a narrow margin column behind a
  vertical rule.
- **Agents, events and asks**: ruled rows with no cards. A handoff and a coordinator's
  turn endings hang off a thin left rule. An ask carries a coloured edge for its kind.
- **Type and colour**: Inter at 15px for text and a system serif (Iowan Old Style,
  Charter, Georgia) at 34px for page titles and 20px for section headings; a warm paper
  background with the same colour for every surface; near-black ink with one ink-blue
  accent for links and the primary action.
- **Round-6 findings**: autopilot switches write On or Off; board goals open in full on
  click, tap or key; empty coordinators read "No coordinator"; event marks show the state
  an event leaves; agent rows keep the same order of facts. The temporary-board note moves
  into the page subtitle under the title.

## What it trades away

- The 880px column and generous spacing show fewer rows per screen than a full-width
  table; wide screens leave empty margins.
- Serif headings read as editorial rather than as a tool, and the system serif differs
  between platforms.
- Without boxes, the edge of a group rests on rules and spacing alone, which is weaker on
  low-contrast displays.
- The per-board list of the sidebar is gone: boards are one click away on Boards.
