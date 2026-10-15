// A status page's monthly uptime report. Everything shown comes from
// `/data.json` (see `reports::Report`), already formatted.

#let data = json("/data.json")
#let accent = rgb(data.accent)
#let ink = rgb("#1f2330")
#let muted = rgb("#5d6475")
#let rule = rgb("#dfe2e8")
#let level-colors = (
  up: rgb("#2f9e5b"),
  warn: rgb("#d39a1c"),
  down: rgb("#d64545"),
  empty: rgb("#e4e7ec"),
  future: white,
)
#let heading-font = if data.look == "pixel" { "Pixelify Sans" } else { "IBM Plex Sans" }

#set document(title: data.title + " uptime report, " + data.label, author: data.title)
#set page(
  paper: "a4",
  margin: (x: 18mm, top: 16mm, bottom: 20mm),
  footer: context [
    #set text(size: 7.5pt, fill: muted)
    #line(length: 100%, stroke: 0.5pt + rule)
    #v(-4pt)
    #grid(
      columns: (1fr, auto),
      [Generated #data.generated · Powered by uptimestatus],
      [Page #counter(page).display() of #counter(page).final().first()],
    )
  ],
)
#set text(font: "IBM Plex Sans", size: 9.5pt, fill: ink)
#set par(leading: 0.6em)
#show heading: set text(font: heading-font, weight: "semibold", fill: ink)
#show heading.where(level: 2): it => block(above: 16pt, below: 8pt)[
  #set text(size: 12pt)
  #it.body
  #v(-6pt)
  #line(length: 100%, stroke: 1.2pt + accent)
]

#let pill(label, color) = box(
  inset: (x: 5pt, y: 2pt),
  radius: 3pt,
  fill: color.lighten(82%),
  stroke: 0.5pt + color,
  text(size: 8pt, weight: "semibold", fill: color.darken(25%), label),
)
#let verdict(met) = if met == none { [—] } else if met {
  pill("Met", level-colors.up)
} else {
  pill("Missed", level-colors.down)
}
#let mono(body) = text(font: "JetBrains Mono", size: 8.5pt, body)

// ── Header ───────────────────────────────────────────────────────────────
#block(width: 100%, fill: accent.lighten(90%), stroke: (left: 4pt + accent), inset: 12pt, radius: 2pt)[
  #grid(
    columns: if data.logo != none { (auto, 1fr) } else { (1fr,) },
    column-gutter: 12pt,
    align: horizon,
    ..if data.logo != none { (image(data.logo, height: 34pt),) },
    [
      #text(font: heading-font, size: 20pt, weight: "semibold", data.title) \
      #text(size: 11pt, fill: muted)[Uptime report · #data.label#if data.to_date [ (month to date)]]
    ],
  )
]

// ── Summary ──────────────────────────────────────────────────────────────
#v(6pt)
#grid(
  columns: (1fr, 1fr, 1fr),
  column-gutter: 10pt,
  ..(
    ([Overall uptime], text(size: 18pt, weight: "semibold", font: "JetBrains Mono", if data.overall != none { data.overall } else { "—" })),
    ([Uptime target], if data.target != none [
      #text(size: 18pt, weight: "semibold", font: "JetBrains Mono", data.target) #h(4pt) #verdict(data.overall_met)
    ] else [#text(size: 12pt, fill: muted)[None set]]),
    ([Incidents], text(size: 18pt, weight: "semibold", font: "JetBrains Mono", str(data.incidents.len()))),
  ).map(((label, value)) => block(width: 100%, inset: 10pt, stroke: 0.5pt + rule, radius: 3pt)[
    #text(size: 8pt, fill: muted, upper(label)) \
    #value
  ])
)
#if data.excused > 0 [
  #v(4pt)
  #text(size: 8.5pt, fill: muted)[#data.excused failed checks were excluded from uptime by an administrator this month (recorded in the audit trail).]
]

// ── Components ───────────────────────────────────────────────────────────
== Components
#table(
  columns: (1fr, auto, auto, auto, auto),
  stroke: (x, y) => if y == 0 { (bottom: 0.8pt + ink) } else { (bottom: 0.5pt + rule) },
  inset: (x: 5pt, y: 5pt),
  align: (left, right, right, right, center),
  table.header(..([Component], [Uptime], [Downtime], [Maintenance], [Target]).map(c => text(size: 8pt, weight: "semibold", fill: muted, upper(c)))),
  ..for section in data.sections {
    (table.cell(colspan: 5, text(weight: "semibold", section.name)),)
    for c in section.components {
      (
        [#h(6pt)#c.name],
        mono(if c.uptime != none { c.uptime } else { "—" }),
        mono(c.downtime),
        mono(c.maintenance),
        verdict(c.met),
      )
    }
  },
)

// ── Daily history ────────────────────────────────────────────────────────
== Daily history
#text(size: 8pt, fill: muted)[One bar per day (UTC): #box(rect(width: 7pt, height: 7pt, fill: level-colors.up)) operational #h(4pt) #box(rect(width: 7pt, height: 7pt, fill: level-colors.warn)) degraded or minor outage #h(4pt) #box(rect(width: 7pt, height: 7pt, fill: level-colors.down)) outage #h(4pt) #box(rect(width: 7pt, height: 7pt, fill: level-colors.empty)) no data]
#v(4pt)
#for section in data.sections {
  for c in section.components {
    block(breakable: false, below: 6pt)[
      #grid(
        columns: (34%, 1fr),
        align: horizon,
        text(size: 8.5pt, c.name),
        grid(
          columns: (1fr,) * c.bars.len(),
          column-gutter: 1.2pt,
          ..c.bars.map(level => rect(
            width: 100%,
            height: 12pt,
            radius: 1pt,
            fill: level-colors.at(level),
            stroke: if level == "future" { 0.4pt + rule } else { none },
          )),
        ),
      )
    ]
  }
}

// ── Incidents ────────────────────────────────────────────────────────────
== Incidents
#if data.incidents.len() == 0 [
  #text(fill: muted)[No incidents this month.]
] else [
  #table(
    columns: (1fr, auto, auto, auto),
    stroke: (x, y) => if y == 0 { (bottom: 0.8pt + ink) } else { (bottom: 0.5pt + rule) },
    inset: (x: 5pt, y: 5pt),
    table.header(..([Incident], [Impact], [Started], [Duration]).map(c => text(size: 8pt, weight: "semibold", fill: muted, upper(c)))),
    ..for i in data.incidents {
      (
        [#text(weight: "semibold", i.title) \ #text(size: 8pt, fill: muted, i.components.join(", "))],
        i.impact,
        mono(i.started),
        mono(i.duration),
      )
    },
  )
]

#if data.maintenance.len() > 0 [
  == Maintenance
  #table(
    columns: (1fr, auto, auto),
    stroke: (x, y) => if y == 0 { (bottom: 0.8pt + ink) } else { (bottom: 0.5pt + rule) },
    inset: (x: 5pt, y: 5pt),
    table.header(..([Window], [Starts], [Ends]).map(c => text(size: 8pt, weight: "semibold", fill: muted, upper(c)))),
    ..for m in data.maintenance { ([#m.title], mono(m.starts), mono(m.ends)) },
  )
]

// ── Methodology ──────────────────────────────────────────────────────────
== How uptime is measured
#set text(size: 8.5pt, fill: muted)
#for line in data.methodology [
  #line
  #parbreak()
]
