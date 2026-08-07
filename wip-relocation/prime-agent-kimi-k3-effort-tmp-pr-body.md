Kimi K3 accepts the standard reasoning effort levels, but its generated model metadata marked every level except max as unsupported. Requests for high effort were therefore clamped to max before reaching the provider.

Remove the unsupported-level overrides while retaining explicit xhigh and max mappings. Regenerate the provider catalogs and update the regression coverage to require every supported level.
