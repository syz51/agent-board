# Design 2: Primer

A familiar code-host look in the spirit of GitHub's Primer: a two-tier header with
underlined tabs, a centred 1280px page, and everything framed in bordered boxes with grey
header bars.

## Key choices

- **Layout**: no sidebar. The header holds the product and the connection, then the four
  sections as tabs, the current one underlined in coral. Pages sit in a centred column
  with a ruled page header.
- **Boxes**: task groups, task sections (Description, Acceptance, Evidence, Notes,
  History) and the agents list are bordered boxes with a grey bar for the heading and a
  count pill. Lists inside run to the box edges.
- **Boards as a repository list**: a large link-blue name with its slug pill, the goal
  under it, autopilot at the right, then one line of facts: tasks by status in words,
  coordinator and open asks. No column heads.
- **Task as an issue**: a 32px regular-weight title, a state line under it (status,
  priority, owner, opened), and a sidebar of facts with small bold labels above values.
- **Asks as comments**: a grey header with kind, number, author, time and place; the
  question; the answer form in a grey footer.
- **Type and colour**: the system font stack at 14px, Primer's greys, link blue for
  links, green for the primary action. Pills and status badges carry a thin border.
- **Round-6 findings**: autopilot switches write On or Off; board goals open in full on
  click, tap or key; empty coordinators read "No coordinator"; event marks show the state
  an event leaves; agent rows put identity, work and facts in the same three columns.

## What it trades away

- Boxes and bars cost vertical space: fewer tasks fit on one screen than in a flat list.
- It looks like a code host, not its own product; the familiarity is the point.
- Without a sidebar, boards are one click further away; the boards list is the hub.
