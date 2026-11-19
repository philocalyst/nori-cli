# Adds `pkgs.nori`, built from this checkout.
self: final: _prev: {
  nori = final.callPackage "${self}/nori-rs" {
    opencode = final.opencode;
  };
}
