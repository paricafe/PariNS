# PariNS beUI adaptations

These four components are adapted from the free, MIT-licensed [beUI](https://beui.dev/) source by Saurabh Chauhan. The registry endpoints were inspected on 2026-09-23; the commit links record the latest GitHub file revisions observed then (the registry adds its own source annotation, so its bytes need not match the repository file). The upstream registry is source distribution, not a runtime dependency. The original copyright and permission notice are retained in [LICENSE.beui](LICENSE.beui).

| Component | Official registry | Upstream file commit | PariNS adaptation |
| --- | --- | --- | --- |
| Button | https://beui.dev/r/button | [`fa3393a`](https://github.com/starc007/ui-components/blob/fa3393a3891723d4d2daec1f1d82ead175442f44/components/motion/button/base.tsx) | Keep Motion spring press; remove ripple, magnetic/metallic variants, hover scale, demo state; use compact semantic variants and native button props. |
| Tabs | https://beui.dev/r/tabs | [`26b92e3`](https://github.com/starc007/ui-components/blob/26b92e3d60c6bdf39f967b712348b56dacdf263e/components/motion/tabs.tsx) | Controlled `items/value/onChange` API and moving underline; add roving focus, Arrow/Home/End keyboard selection; omit demo scrolling masks, runtime clip styles and panel renderer. |
| Switch | https://beui.dev/r/switch | [`a0b4598`](https://github.com/starc007/ui-components/blob/a0b45987787d43446a000267217116320b1c9a2f/components/motion/switch.tsx) | Keep controlled Motion thumb; remove disabled shake and stretch; keep native button, labels and focus styling. |
| Drawer | https://beui.dev/r/drawer | [`c89ba59`](https://github.com/starc007/ui-components/blob/c89ba59090bdc6f9bb7e533570ae5f74fcd2a0b0/components/motion/drawer.tsx) | Keep restrained Motion slide-in; use native modal `<dialog>` for focus containment/background inert, restore focus, Escape/backdrop/close handling; unmount immediately on close to clear sensitive content. |

The shared ease/spring values in `motion.ts` are selected from upstream [`lib/ease.ts`](https://github.com/starc007/ui-components/blob/fa3393a3891723d4d2daec1f1d82ead175442f44/lib/ease.ts). Styles are build-time CSS using PariNS semantic tokens; no runtime style tag, CDN, second animation library, or additional package is used. Motion applies transform to elements for the short interactions; reduced-motion disables those transforms where appropriate.

For bilingual use, pass the translated `Drawer.closeLabel` and a concise `Tabs.label` when the tab group needs an accessible name. `Button` defaults to `type="button"`; submit actions must pass `type="submit"` explicitly. The parent owns selected tab value, switch value, and drawer open state.
