# Design 1: Linear

A quiet, dense work tool in the spirit of Linear: a tinted sidebar with the content on an
inset panel, and lists that run edge to edge across it.

## Key choices

- **Layout**: a 224px sidebar (navigation, then every board with its open asks) and an
  inset content panel with a soft outline. Lists, the boards table and task groups run
  across the full panel width, with group bands and rows meeting its edges; text lines
  up with the page title through one gutter variable (`--gutter`), set on the panel.
- **Rows**: one row model for lists. A row has one main line and at most one line of
  detail; states sit inline beside names, not stacked under them. Fixed columns size to
  their content, and the last column takes what is left.
- **Density**: 36px task rows and 38px group bands. Boards rows are one 68px height: the
  name, slug, states and open asks (a tinted pill, only when there are some) share one
  line, none of them cut, the goal keeps to one line (a click shows the rest), the task tally to one (on phones it
  wraps into three even columns, never one count alone) and the coordinator to two; cells
  start level with the name, and a board with no coordinator says "None".
  Agents are a two-column table: who (in a 300px column), with its state, when last seen
  and where it runs in two quiet lines, its machine, then its repository and branch in the
  id face a step smaller; then what it is working on, with its handoff. Priority labels share one weight; the
  bars carry the level.
- **Type**: Inter at 13px body, 12.5px meta, 24px page titles and 28px task titles; prose
  stops at a 100-character measure.
- **Color and surfaces**: cool greys, a graphite primary action and one violet signal for
  links, selection and unseen marks. The panel's outline is the only elevation. Status
  colours stay, always with a shape and a word: board tallies spell out "3 done 1 review"
  after the status icon of each bar colour, the same shapes as on a board. Agent and board states are one style everywhere: a dot and
  a word, no pill.
- **Navigation**: Home, Needs you and Agents; the sidebar on wide screens, a top bar and
  bottom tabs on phones. Counts there are what waits on the user, in one pill: filled amber
  for the total (Needs you), tinted amber for a board's share of it. Home has no count; its
  Since you last looked says what is new. A count that cannot load does not show (the
  banner and footer say why), and boards that cannot load are placeholder rows. On a board or a task the sidebar marks the
  board, not Home.
- **Home**: what the user follows, in sections they collapse and reorder: Needs you (the
  asks, one name for them in the nav, the section and its "See all" link; edge to edge like
  every list on Home, with a short bar in each kind's colour on the content edge, and an
  Answer cue on hover), the boards,
  tasks in progress across boards (Doing and Review under their own headings), and what
  changed since they last looked. Finished tasks there are one-line rows whose summary
  opens on demand. Needs you holds only asks, the things that wait on the user. Board
  status groups collapse, with the chevron left of the label as on Home, and done starts
  collapsed. What a task waits for (blockers, a start date) is muted text with an icon;
  tinted chips are tags only, one shape in tones: neutral grey, amber for a warning. Task
  rows sit under their status's heading and do not repeat its icon. A board with nothing to
  filter has no filter bar. On phones a task row's time sits on its title line.
- **Asks**: an ask card's left border takes its kind's colour. A filled Send, the same
  primary button as Try again, closes the answer group at its left, under the options and a
  two-line field that grows, so it plainly sends either; Cmd or Ctrl with Enter sends from
  the field. A single ask is titled with its kind ("Decision · Ask #9"), across the content
  width with its prose at a reading measure, and its task and the asking agent's handoff in
  the side panel beside it, split by a rule, level with the card at top and bottom, or
  below it on narrow screens.
- **Side panel**: a task's properties and an ask's context sit in one kind of card, 300px
  wide beside the main column, its sections split by rules, and kept in view while the
  main column scrolls on wide screens.
- **Failures**: while the connection banner shows, only the banner warns; a failed page is
  a neutral placeholder that says it loads again once the connection is back; its icon
  tile stays error-tinted, so it never reads as an empty state. "Try again" is a filled
  button.
- **Empty states**: a dashed outline with room around the message, so they read as a place
  for content rather than a card.

## What it trades away

- Small type and edge-to-edge rows favour scanning on a desktop over reading on a phone.
- Cards are gone from lists, so a row's extent shows only by its rules and hover.
- A board with a long name and slug takes a second line for them on a narrow desktop.
