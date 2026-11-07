{
  description = "reflection_core: Reflection notes engine (Rust) for Logos Basecamp, on loam_core";

  inputs = {
    # Same builder as the loam_core it depends on (Basecamp 0.3.x stack).
    logos-module-builder.url = "github:logos-co/logos-module-builder/16e2f6bd3c06a5119f8c34448933de183205646c";
    # loam_core 0.7.3 (vpavlin/loam-basecamp feat/relay-default): only its contract is used here.
    loam_core.url = "github:vpavlin/loam-basecamp/d5c72af7e07490079f26a41ac606b27cc4d6da2a?dir=core";
    loam_core.inputs.logos-module-builder.follows = "logos-module-builder";
  };

  outputs = inputs@{ logos-module-builder, ... }:
    logos-module-builder.lib.mkLogosModule {
      src = ./.;
      configFile = ./metadata.json;
      flakeInputs = inputs;
    };
}
