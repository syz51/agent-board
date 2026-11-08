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
- **Density**: 36px task rows and 38px group bands. Boards rows keep the name, slug and
  badges on one line and the goal to one line. Agents are a three-column table: who, with
  state and, on its own line, when last seen; what it is working on, with its handoff (at most 560px);
  and where it runs, as label and value pairs.
- **Type**: Inter at 13px body, 12px meta, 24px page titles and 28px task titles; prose
  stops at a 100-character measure.
- **Color and surfaces**: cool greys, a graphite primary action and one violet signal for
  links, selection and unseen marks. The panel's outline is the only elevation. Status
  colours stay, always with a shape and a word: board tallies spell out "3 done 1 review"
  after a dot of each bar colour. Agent and board states are one style everywhere: a dot and
  a word, no pill.
- **Navigation**: Home, Needs you and Agents; the sidebar on wide screens, a top bar and
  bottom tabs on phones. Needs you counts the open asks; a dot on Home marks unseen updates.
- **Home**: what the user follows, in sections they collapse and reorder: waiting asks,
  tasks in progress across boards, what changed since they last looked, and the boards.
  Needs you holds only asks, the things that wait on the user. Board status groups
  collapse, and done starts collapsed.
- **Asks**: a filled Send closes the answer group, under the options and the text field,
  so it plainly sends either. A single ask is titled with its kind ("Decision · Ask #9"),
  at a reading width, with its task and the asking agent's handoff below it, or beside it
  on the widest screens.
- **Failures**: while the connection banner shows, only the banner warns; a failed page is
  a neutral placeholder that says it loads again once the connection is back. "Try again"
  is a filled button.

## What it trades away

- Small type and edge-to-edge rows favour scanning on a desktop over reading on a phone.
- Cards are gone from lists, so a row's extent shows only by its rules and hover.
- A paused coordinator's end time can wrap its boards row to a second line.
