{
  description = "p2panda: the view of p2panda_core in Logos Basecamp (status, bridge approvals)";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder/16e2f6bd3c06a5119f8c34448933de183205646c";
    # Only the core's contract (LIDL) is used.
    p2panda_core.url = "github:vpavlin/reflection-basecamp/7ccf2ac3b4a9c28fc4d45a8dab7fc2b69d059fd0?dir=p2panda_core";
    p2panda_core.inputs.logos-module-builder.follows = "logos-module-builder";
  };

  outputs = inputs@{ logos-module-builder, ... }:
    logos-module-builder.lib.mkLogosQmlModule {
      src = ./.;
      configFile = ./metadata.json;
      flakeInputs = inputs;
    };
}
