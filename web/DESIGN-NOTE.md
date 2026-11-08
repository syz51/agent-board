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
- **Density**: 36px task rows and 38px group bands. Boards rows never cut the name or
  slug (badges or a long slug take a second line), keep the goal to two lines and the task
  tally to one. Agents are a two-column table: who, with
  its state, when last seen and one quiet line of where it runs (cut with an ellipsis,
  whole on hover); then what it is working on, with its handoff. Priority labels share one
  weight; the bars carry the level.
- **Type**: Inter at 13px body, 12.5px meta, 24px page titles and 28px task titles; prose
  stops at a 100-character measure.
- **Color and surfaces**: cool greys, a graphite primary action and one violet signal for
  links, selection and unseen marks. The panel's outline is the only elevation. Status
  colours stay, always with a shape and a word: board tallies spell out "3 done 1 review"
  after a dot of each bar colour. Agent and board states are one style everywhere: a dot and
  a word, no pill.
- **Navigation**: Home, Needs you and Agents; the sidebar on wide screens, a top bar and
  bottom tabs on phones. Counts there are what waits on the user, in one pill: filled amber
  for the total (Needs you), tinted amber for a board's share of it. Home has no count; its
  Since you last looked says what is new. A count that cannot load is an offline mark, and
  boards that cannot load are placeholder rows. On a board or a task the sidebar marks the
  board, not Home.
- **Home**: what the user follows, in sections they collapse and reorder: waiting asks (a
  left edge in each kind's colour, as on Needs you, and an Answer cue on hover), the boards,
  tasks in progress across boards (Doing and Review under their own headings), and what
  changed since they last looked. Finished tasks there are one-line rows whose summary
  opens on demand. Needs you holds only asks, the things that wait on the user. Board
  status groups collapse, with the chevron left of the label as on Home, and done starts
  collapsed. What a task waits for (blockers, a start date) is muted text with an icon;
  outlined chips are tags only. A board with nothing to filter has no filter bar. On phones
  a task row's time sits on its title line.
- **Asks**: a filled Send closes the answer group at its left, under the options and a
  two-line field that grows, so it plainly sends either; Cmd or Ctrl with Enter sends from
  the field. A single ask is titled with its kind ("Decision · Ask #9"), across the content
  width with its prose at a reading measure, and its task and the asking agent's handoff in
  one card beside it, split by a rule, or below it on narrow screens.
- **Failures**: while the connection banner shows, only the banner warns; a failed page is
  a neutral placeholder that says it loads again once the connection is back; its icon
  tile stays error-tinted, so it never reads as an empty state. "Try again" is a filled
  button.

## What it trades away

- Small type and edge-to-edge rows favour scanning on a desktop over reading on a phone.
- Cards are gone from lists, so a row's extent shows only by its rules and hover.
- A paused coordinator's end time can wrap its boards row to a second line.
