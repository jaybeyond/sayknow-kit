import type { FeatureId } from "../data/site"
import type { LandingCopy } from "./landing"

export type Point = { title: string; body: string }

export type FeatureCopy = {
  /** Short name used in navigation and cards. */
  name: string
  /** One sentence: what it is for. */
  summary: string
  /** What it does, one fact per point. */
  points: Point[]
  /** Limits worth knowing before relying on it (platform, requirements). */
  notes: string[]
}

export type Copy = {
  meta: { title: string; description: string }
  nav: {
    home: string
    features: string
    download: string
    changelog: string
    shortcuts: string
    feedback: string
    privacy: string
    github: string
    language: string
    skip: string
    /** The theme switch's label, named for where it takes you. */
    themeDark: string
    themeLight: string
  }
  home: {
    tagline: string
    latest: string
    /** "{date}" is replaced with the release date. */
    released: string
    macButton: string
    macDetail: string
    winButton: string
    winDetail: string
    otherFormats: string
    facts: Point[]
    featuresLink: string
    whatsNew: string
  }
  features: {
    title: string
    intro: string
    more: string
    notesTitle: string
    allFeatures: string
    items: Record<FeatureId, FeatureCopy>
  }
  download: {
    title: string
    intro: string
    fileHeading: string
    fileCol: string
    sizeCol: string
    macTitle: string
    macSteps: string[]
    /** Lead-in for the quarantine command, which the page renders as code. */
    macTerminal: string
    winTitle: string
    winSteps: string[]
    checksumTitle: string
    checksumBody: string
    securityTitle: string
    securityBody: string[]
    requirementsTitle: string
    requirements: string[]
    uninstallTitle: string
    uninstall: string[]
    updatesTitle: string
    updatesBody: string
    allReleases: string
  }
  changelog: { title: string; intro: string; older: string }
  shortcuts: {
    title: string
    intro: string
    action: string
    local: string
    note: string
  }
  privacy: { title: string; intro: string; sections: Point[] }
  feedback: {
    title: string
    intro: string
    bugTitle: string
    bugBody: string
    bugCta: string
    ideaTitle: string
    ideaBody: string
    ideaCta: string
    beforeTitle: string
    before: string[]
    logsTitle: string
    logs: string[]
    browse: string
    account: string
  }
  footer: { license: string; source: string }
  landing: LandingCopy
}

/** What each locale file writes; the landing copy is kept in landing.ts. */
export type LocaleCopy = Omit<Copy, "landing">
