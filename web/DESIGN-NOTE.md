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
- **Density**: 36px task rows and 38px group bands. Agents are a three-column table: who,
  with state and when last seen; what it is working on, with its handoff (at most 560px);
  and where it runs, as label and value pairs.
- **Type**: Inter at 13px body, 12px meta, 24px page titles and 28px task titles; prose
  stops at a 100-character measure.
- **Color and surfaces**: cool greys, a graphite primary action and one violet signal for
  links, selection and unseen marks. The panel's outline is the only elevation. Status
  colours stay, always with a shape and a word: board tallies spell out "3 done 1 review".
- **Navigation**: the sidebar on wide screens; a top bar and bottom tabs on phones. Needs
  you shows one mark: its count, or a dot for unseen updates when nothing waits.
- **Asks**: a filled Send beside the answer field; a single ask shows its task and the
  asking agent's handoff beside it.
- **Failures**: while the connection banner shows, a failed page uses its plug and tone.

## What it trades away

- Small type and edge-to-edge rows favour scanning on a desktop over reading on a phone.
- Cards are gone from lists, so a row's extent shows only by its rules and hover.
- A paused coordinator's end time can wrap its boards row to a second line.
