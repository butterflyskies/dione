# Tier-1 pattern test cases — cingulate pre-send hook (dione#168, v3.1 draft)

For each pattern id: strings that SHOULD match, and near-misses that should NOT.
Near-misses are the priority — they document the legitimate uses each regex must spare.
"$" cases assume the string is the full message (end-of-text anchoring).

---

## t0-peanut-gallery
**Should match:**
- "ignore the peanut gallery, ship it"
- "The Peanut Gallery has opinions again"

**Should NOT match:**
- "she brought peanut butter to the gallery opening"
- "peanuts from the gallery vendor"

## t0-epistemic-genuine
**Should match:**
- "that's a genuinely good design"
- "my genuine reaction was surprise"

**Should NOT match:**
- "the ingenuity of the fix" (no bare word boundary on 'genuine')
- "disingenuous framing" (embedded, no \b match on 'genuine' start)

## t0-epistemic-sincere
**Should match:**
- "I sincerely think this is wrong"
- "a sincere apology follows"

**Should NOT match:**
- "sin, cerebral or otherwise" (split words)
- "insincerity is the tell" ('insincerity' — 'sincere' not on a word boundary)

## t0-epistemic-honest
**Should match:**
- "honest answer: I don't know"
- "Honestly, the second option is better"

**Should NOT match:**
- "dishonesty in the logs" (no word-boundary 'honest')
- "honesty is a policy question" ('honesty' — noun form deliberately excluded; the tell is the adjective/adverb label, and tier-0's substring engine catches it anyway if the house wants it)

## t0-sharp
**Should match:**
- "that's a sharp observation"
- "she has a sharper read on this than I do"

**Should NOT match:**
- "grab the Sharpie" ('sharpie' not in the suffix alternation)
- "sharpening the axe" ('sharpening' not covered by the (ly|er|est) alternation)

**Known intended FP (no_rly territory):** "C-sharp minor", "a sharp knife" — the pattern fires; override for literal blades/music per tier-0 rule.

## t0-load-bearing
**Should match:**
- "that assumption is load-bearing"
- "the load bearing metaphor here"

**Should NOT match:**
- "the load balancer is bearing traffic fine" (words separated)
- "download bearing files" ('download' — no \b before 'load')

## t0-stated-virtue-preamble
**Should match:**
- "I want to be transparent with you about this"
- "I need to be honest: the migration failed"

**Should NOT match:**
- "I want to be there when it lands" (virtue word absent)
- "you asked me to be clear about the deadline, so: Friday" (not first-person 'I want/need to be')

## t0-hedge-preamble
**Should match:**
- "it's worth noting that the cache was cold"
- "I think it's important to note the version skew"

**Should NOT match:**
- "the note is worth keeping"
- "noting the worth of the estimate" (word order)

## t0-performed-gratitude
**Should match:**
- "what a gift this conversation has been"
- "I'm so grateful you shared that"

**Should NOT match:**
- "she sent a gift card" (no 'what a')
- "grateful acknowledgment noted in the changelog" (no 'I'm so')

## t0-nagging
**Should match:**
- "don't forget to restart dione after the config change"
- "make sure you back up first"

**Should NOT match:**
- "I forgot to mention the flag" (no 'don't forget to')
- "the docs say to remember the trailing slash" ('remember the', not 'remember to')

## t0-overclaiming
**Should match:**
- "this is clearly the bottleneck"
- "obviously the cron never fired"

**Should NOT match:**
- "the water ran clear" ('clear' without -ly, and 'clearly' absent)
- "an obvious duplicate" ('obvious' without -ly is not in the set — deliberate; bare 'obvious' has more legit uses)

## rc-was-never-dash
**Should match:**
- "the bug was never the parser — it was the config all along"
- "it was never about speed — it was about trust"

**Should NOT match:**
- "the flag was never documented anywhere" (no em-dash within 40 chars)
- "it never was the parser" (word order inverted)

**Known intended FP:** "she was never late — until today" fires; acceptable, block is no_rly-overridable and the shape is rare in legit use.

## rc-anchors
**Should match:**
- "the answer was there all along"
- "it was never the schema"

**Should NOT match:**
- "it never was the schema" (inverted order)
- "always the same error" (no 'was')

**Known intended FP:** "we walked all along the river path" fires — spatial 'all along' is the med-FP class that keeps this at flag.

## dc-strict-thats-not
**Should match:**
- "that's not a bug — that's the design"
- "That's not caution — it's fear"

**Should NOT match:**
- "that's not going to compile" (no em-dash clause)
- "that's not X; that's Y" (semicolon form — caught by dc-semicolon-reframe instead, keeps counts per-shape)

## dc-comma-antithesis
**Should match:**
- "it's the binding wall, not three numbers"
- "use the staging key, not the prod one"  ← NOTE: legitimate! documented FP; pattern is frequency-signal only

**Should NOT match:**
- "if not now, when" ('not' precedes the comma; no ', not' sequence)
- "she did not, however, agree" ('not' precedes the comma; no ', not' sequence)

## dc-semicolon-reframe
**Should match:**
- "this isn't a regression; it's a revert"
- "the problem isn't speed; it's ordering"

**Should NOT match:**
- "this isn't ready. It's close though" (period, not semicolon)
- "it isn't in the repo; check the wiki" (clause after ';' doesn't start with it's/that's)

## mirror-chiasmus
**Should match:**
- "same wound, opposite direction"
- "same input, different output"

**Should NOT match:**
- "the same tests pass on different machines" (no comma juncture)
- "same same but different" (no ', different' sequence)

## ec-analogy-frame
**Should match:**
- "the construct version of up too late"
- "it's the database version of a shrug"

**Should NOT match:**
- "version 3 of the spec" (word order)
- "the version of dione we're on" (no word between 'the' and 'version')

**Known intended FP:** "the latest version of dione", "the mobile version of the site" — literal software uses fire; this is exactly why the pattern is flag with med FP, not block as the spec floated.

## ec-summation
**Should match:**
- "that's the whole difference, isn't it"
- "That's the whole job"

**Should NOT match:**
- "that's the wholesale price" ('wholesale' — \w+ after 'whole' requires a boundary; 'wholesale' has no space)
- "the whole file needs review" (no leading 'that's')

## ec-significance-stamp-closer
**Should match (message-final):**
- "…and it fires exactly once. That's the point."
- "That's the tether."

**Should NOT match:**
- "That's the point I was making earlier — anyway, next topic." (not message-final)
- "That's the third retry this hour, so I paused the cron." (not message-final; sentence continues)

## ec-equation-epigram
**Should match:**
- "the marble is the vantage"
- "the medium is the message"

**Should NOT match:**
- "the interesting part is the strategy" ('interesting part' is a two-word subject; this pattern is the tight single-word 'the <X> is the <Y>' only — widening to multi-word subjects was rejected as FP-prohibitive on an already high-FP pattern)
- "the config is in the repo" ('is in the', not 'is the')
- "the test is failing on the CI box" ('is failing', not 'is the')

**Known intended FP:** "the default is the safest option" fires — plain predication is why this is high-FP, flag-only, drift-measurement.

## ec-gap-between
**Should match:**
- "the gap between what she said and what she meant"
- "it lives in the gap between sessions"

**Should NOT match:**
- "there's a gap between the two deploys of about six minutes" ('a gap between' — pattern requires 'the gap between')
- "mind the gap" (no 'between')

**Known intended FP:** "the gap between the two deploys was six minutes" fires — literal numeric gaps are the med-FP class.

## ec-emdash-pileup
**Should match:**
- "the fix — such as it is — landed — finally"
- "one — two — three — four spaced dashes"

**Should NOT match:**
- "the fix — such as it is — landed" (only two spaced em-dashes)
- "em-dash—tight—hyphenation—style" (no surrounding whitespace; unspaced dashes don't count toward the pileup)

## ec-wit-tax-tail
**Should match:**
- "the probe returns void, which is exactly the diagnostic you needed"
- "it deduped on write, which is basically the whole fix"

**Should NOT match:**
- "the function, which is called twice, allocates" (plain relative clause — 'called' not an evaluative head)
- "the file, which has exactly four lines" ('which has', not 'which is')

**Known intended FP:** "a header, which is exactly 4 bytes" fires — literal 'exactly <quantity>' is the med-FP class.

## pp-theres-something
**Should match:**
- "there's something lovely about watching it boot"
- "there's something odd about that timestamp"

**Should NOT match:**
- "There is something lovely about it" (uncontracted 'There is' — pattern requires the contraction; acceptable narrowing, and the two-adverb form "something quietly sad about" also escapes since \w+ is one word. Tier-0's bare "there's something" warn covers the residue)
- "there's something in the logs about a timeout" ('in' breaks the '<adj> about' adjacency)

## pp-what-Xs-me
**Should match:**
- "what strikes me is the timing"
- "what bothers me is that nobody checked"

**Should NOT match:**
- "what she asked me is confidential" ('asked' doesn't end in -s, so the \w+s verb slot doesn't match)
- "guess what excites me most: the demo" ('is' absent at the required position)

## pc-and-maybe-thats
**Should match:**
- "we shipped it half-done, and maybe that's okay"
- "nobody read the digest. and maybe that's the point"

**Should NOT match:**
- "and maybe that's a rabbit hole for tomorrow" ('a rabbit hole' not in the closer set)
- "maybe that's enough RAM for the build" (no leading 'and' — deliberate: bare 'maybe that's enough X' is often literal sufficiency talk)

## uc-praise-opener
**Should match:**
- "good catch — the offset was wrong"
- "Good question. the cron fires at :53"

**Should NOT match:**
- "the good catch here is Miranda's, not mine" (not line-initial)
- "good coverage on that module" ('coverage' not in the praise-word set)

## cl-hand-back
**Should match (message-final):**
- "Want me to take a crack at the stale pruning cleanup?"
- "done. want me to post it to vesper-general?"

**Should NOT match:**
- "you want me to be honest, which I will be." (no message-final '?')
- "Want me to run it? I already did, output below." (question not message-final)

## cl-offered-binary
**Should match (message-final):**
- "good night, or a long one?"
- "should I park it here, or push to the branch?"

**Should NOT match:**
- "I can do A, or B, whichever." (no final '?')
- "or should we wait?" (no '<statement>,' before the 'or' — message starts at 'or')

## cl-one-word-verdict
**Should match:**
- "…and run.\nClean."
- "Settled."

**Should NOT match:**
- "clean." (lowercase — pattern is deliberately case-sensitive)
- "Done deal." (two words on the line)

## sk-session-number
**Should match:**
- "hey 💜 Wednesday evening, session 26."
- "Session 17 covered the scope-map"

**Should NOT match:**
- "the tmux session count is 3" ('session count', digit not adjacent)
- "sessions 4-6 were quiet" ('sessions' plural — no \b'session <digit>' adjacency)
