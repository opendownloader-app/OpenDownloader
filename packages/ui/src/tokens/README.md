# Vendored OpenApps design tokens

Copied from the OpenApps design system (`openapps/tokens`) rather than depended on,
because this repository is published on its own: a `file:../../../tokens` dependency
resolves on the machine it was written on and nowhere else.

What is here is the subset a product UI can reach — colour, type, spacing, radius,
elevation, motion, and the three subsetted Geist faces (44 KB for all three). The
editorial weights are deliberately absent; a component asking for 700 gets a synthesised
bold, which is a visible signal that it has left the product type scale.

**The fonts are self-hosted and must stay that way.** Manifest V3 blocks remote font
loads outright, so a `fonts.googleapis.com` link would silently fall back to the system
face inside the extension while looking correct in the web app.

To update: re-copy from the design system. Nothing here is edited locally — anything
this product needs that the system does not provide belongs in `base.css`.
