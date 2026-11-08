// CHANGELOG.md format guard.
//
// Every release section lists its subheadings in the fixed order
// Added > Changed > Fixed > Removed (any subset, never shuffled), so the notes
// read the same way in every GitHub Release. A heading may carry an emoji
// accent; only the trailing word counts.
//
// A bullet lives on one line, however long: GitHub turns every newline in a
// Release body into a line break, so a wrapped bullet reaches the Releases
// page as one ragged fragment per source line.
//
// An issue a bullet closes is written as a full link ("[issue #11](url)"),
// never a bare "(#11)": the notes are read outside the repo, where a bare
// number links to nothing.
//
// Every release section opens with the star-and-donate banner, copied exactly
// from the BANNER block in .github/scripts/release_manifest.py, so the
// changelog reads the same as the Releases page.

import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'

const CHANGELOG = fileURLToPath(new URL('../CHANGELOG.md', import.meta.url))
const MANIFEST = fileURLToPath(new URL('../.github/scripts/release_manifest.py', import.meta.url))
const CANONICAL = ['Added', 'Changed', 'Fixed', 'Removed']

const lines = () => readFileSync(CHANGELOG, 'utf8').split('\n')

function banner() {
  const match = readFileSync(MANIFEST, 'utf8').match(/^BANNER = """([\s\S]*?)"""/m)
  expect(match, 'BANNER block in release_manifest.py').not.toBeNull()
  return match[1].trim()
}

// [releaseHeading, body text] per release section.
function bodies() {
  const text = readFileSync(CHANGELOG, 'utf8')
  return text
    .split(/^## /m)
    .slice(1)
    .map((chunk) => {
      const newline = chunk.indexOf('\n')
      return [chunk.slice(0, newline).trim(), chunk.slice(newline + 1).trim()]
    })
}

// [releaseHeading, [subheading names]] per release section.
function sections() {
  const out = []
  let current = null
  for (const line of lines()) {
    if (line.startsWith('## ')) {
      if (current) out.push(current)
      current = [line.slice(3).trim(), []]
    } else if (line.startsWith('### ') && current) {
      const match = line.match(/(Added|Changed|Fixed|Removed)\s*$/)
      expect(match, `unknown subheading ${line} under ${current[0]}`).not.toBeNull()
      current[1].push(match[1])
    }
  }
  if (current) out.push(current)
  return out
}

describe('CHANGELOG.md', () => {
  it('has release sections', () => {
    expect(sections().length).toBeGreaterThan(0)
  })

  it('opens every release section with the star-and-donate banner', () => {
    const expected = banner()
    for (const [heading, body] of bodies()) {
      expect(body.startsWith(expected), `${heading} does not open with the banner`).toBe(true)
    }
  })

  it('lists each subheading once per section', () => {
    for (const [heading, subs] of sections()) {
      expect(new Set(subs).size, `duplicate subheading under ${heading}`).toBe(subs.length)
    }
  })

  it('keeps subheadings in Added > Changed > Fixed > Removed order', () => {
    for (const [heading, subs] of sections()) {
      const expected = CANONICAL.filter((name) => subs.includes(name))
      expect(subs, `${heading} lists subheadings out of order`).toEqual(expected)
    }
  })

  it('never wraps a bullet across lines', () => {
    const all = lines()
    const wrapped = []
    let inRelease = false
    let inFence = false
    all.forEach((line, index) => {
      if (line.startsWith('## ')) inRelease = true
      if (line.trimStart().startsWith('```')) {
        inFence = !inFence
        return
      }
      const previous = index > 0 ? all[index - 1] : ''
      if (!inRelease || inFence || !line.trim() || !previous.trim()) return
      // An indented block may follow a bullet after a blank line; what is
      // banned is text hanging directly off the line above, which is a wrap.
      if (/^(#|- |<)/.test(line) || previous.trimStart().startsWith('<')) return
      wrapped.push(`line ${index + 1}: ${line.trim().slice(0, 60)}`)
    })
    expect(wrapped, 'write each bullet on one line, unwrapped').toEqual([])
  })

  it('cites issues as labeled links', () => {
    const bare = []
    lines().forEach((raw, index) => {
      // Inline code spans quote the rule's own counter-example.
      const line = raw.replace(/`[^`]*`/g, '')
      for (const match of line.matchAll(/#\d+/g)) {
        const named = line.slice(0, match.index).trimEnd().endsWith('issue')
        const after = match.index + match[0].length
        const linked = line.slice(after, after + 80).includes('](http')
        if (!(named && linked)) bare.push(`line ${index + 1}: ${line.trim()}`)
      }
    })
    expect(bare, 'cite issues as [issue #N](https://github.com/.../issues/N)').toEqual([])
  })
})
