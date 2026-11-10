{
  description = "Reflection: collaborative notes in Logos Basecamp (pure QML view of reflection_core)";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder/16e2f6bd3c06a5119f8c34448933de183205646c";
    # Only the core's contract (LIDL) is used. A GitHub ref, not path:../, so CI can build it.
    reflection_core.url = "github:vpavlin/reflection-basecamp/6af80059ce116e3a4fd2bad1999b99f404514def?dir=reflection_core";
    reflection_core.inputs.logos-module-builder.follows = "logos-module-builder";
  };

  outputs = inputs@{ logos-module-builder, ... }:
    logos-module-builder.lib.mkLogosQmlModule {
      src = ./.;
      configFile = ./metadata.json;
      flakeInputs = inputs;
    };
}
