# NixOS module: a Linux robot on the mesh (design.md M43-M45).
#
#   spire-agent      join_token node attestation against the cluster's SPIRE
#                    server; the token, the trust bundle and the addresses
#                    are runtime files, so the image holds no secrets
#   spiffe-helper    as the robot user (uid 1000, the registration selector
#                    `unix:uid:1000`): keeps the robot's X509-SVID, key and
#                    bundle in /run/spiffe
#   mw-envoy         loopback h2c and TCP listeners, mTLS with that SVID to
#                    the Istio ingress gateway (nix/robot/envoy.yaml)
#   mw-robot         the robot app, as the robot user, talking h2c and Zenoh
#                    to the local Envoy only
#
# Runtime file contract, directory /var/lib/mw-robot (see README "Robot VM"):
#   join-token     the token `just helm::robot-join <name> unix:uid:1000` prints
#   bundle.spiffe  the trust bundle, SPIFFE format (ConfigMap
#                  spire-system/spire-bundle, key bundle.spiffe)
#   robot.env      optional KEY=value overrides of the defaults below
#                  (environmentDefaults); read by spire-agent, mw-envoy and
#                  mw-robot
# spire-agent starts only once join-token and bundle.spiffe exist
# (ConditionPathExists); the other units start regardless, so an
# unprovisioned robot still boots with Envoy up and no identity.
{ config, lib, pkgs, ... }:
let
  cfg = config.services.mw-robot;
  stateDir = "/var/lib/mw-robot";
  envFile = "${stateDir}/robot.env";
  spiffeDir = "/run/spiffe";
  certDir = "/run/mw-envoy/certs";
  agentSocket = config.services.spire.agent.settings.agent.socket_path;

  sdsSvid = pkgs.replaceVars ./robot/sds-svid.yaml { inherit certDir; };
  sdsBundle = pkgs.replaceVars ./robot/sds-bundle.yaml { inherit certDir; };
  envoyTemplate = pkgs.replaceVars ./robot/envoy.yaml {
    adminPort = toString cfg.envoy.adminPort;
    httpPort = toString cfg.envoy.httpPort;
    zenohPort = toString cfg.envoy.zenohPort;
    inherit (cfg) trustDomain;
    inherit certDir sdsSvid sdsBundle;
  };
  helperConf = pkgs.replaceVars ./robot/spiffe-helper.conf {
    inherit agentSocket spiffeDir;
  };

  # The runtime variables envoy.yaml takes, for envsubst's allow list.
  envoyVars = [ "MW_GATEWAY_ADDRESS" "MW_GATEWAY_PORT" "MW_GATEWAY_SNI"
                "MW_ZENOH_GATEWAY_PORT" "MW_ZENOH_SNI" ];

  # Renders the Envoy config, and seeds certDir with a throwaway
  # self-signed pair so Envoy starts (and watches certDir) before the robot
  # has an identity. Upstream mTLS fails until the real SVID is copied in.
  envoyPrepare = pkgs.writeShellScript "mw-envoy-prepare" ''
    set -eu
    export PATH=${lib.makeBinPath [ pkgs.envsubst pkgs.openssl pkgs.coreutils ]}
    envsubst '${lib.concatMapStringsSep " " (v: "\${${v}}") envoyVars}' \
      < ${envoyTemplate} > /run/mw-envoy/envoy.yaml
    mkdir -p ${certDir}
    if [ ! -s ${certDir}/svid.pem ]; then
      openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
        -nodes -days 1 -subj /CN=unprovisioned \
        -keyout ${certDir}/svid_key.pem -out ${certDir}/svid.pem 2>/dev/null
      cp ${certDir}/svid.pem ${certDir}/bundle.pem
    fi
  '';

  # Copies spiffe-helper's files into certDir, each through a rename so
  # Envoy's directory watch fires and never reads a half-written file.
  copyCerts = pkgs.writeShellScript "mw-envoy-certs" ''
    set -eu
    export PATH=${lib.makeBinPath [ pkgs.coreutils ]}
    [ -d ${certDir} ] || exit 0
    for f in svid.pem svid_key.pem bundle.pem; do
      install -m 0400 -o mw-envoy -g mw-envoy ${spiffeDir}/$f ${certDir}/.$f.new
      mv -f ${certDir}/.$f.new ${certDir}/$f
    done
  '';
in
{
  options.services.mw-robot = {
    enable = lib.mkEnableOption "the middleware robot stack (SPIRE agent, spiffe-helper, Envoy, robot app)";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The robot app; its `meta.mainProgram` is run.";
    };

    args = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Arguments to the robot app.";
    };

    name = lib.mkOption {
      type = lib.types.str;
      default = "robot";
      description = "The robot's name (MW_ROBOT_NAME); its SPIFFE ID is spiffe://<trustDomain>/robot/<name> by the server's registration.";
    };

    uid = lib.mkOption {
      type = lib.types.int;
      default = 1000;
      description = "The robot user's uid: the workload selector `unix:uid:<uid>` the SPIRE registration names.";
    };

    trustDomain = lib.mkOption {
      type = lib.types.str;
      default = "infra.local";
      description = "SPIFFE trust domain of the SPIRE server and the gateway.";
    };

    spireServerPort = lib.mkOption {
      type = lib.types.port;
      default = 30081;
      description = "The SPIRE server's port (the lab's NodePort passthrough).";
    };

    environmentDefaults = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = {
        MW_SPIRE_SERVER_ADDRESS = "192.168.66.11";
        MW_GATEWAY_ADDRESS = "192.168.66.11";
        MW_GATEWAY_PORT = "30443";
        MW_GATEWAY_SNI = "apps.infra.local";
        MW_ZENOH_GATEWAY_PORT = "30443";
        MW_ZENOH_SNI = "zenohd.apps.infra.local";
      };
      description = "Runtime settings, each overridable by ${envFile}.";
    };

    envoy = {
      adminPort = lib.mkOption { type = lib.types.port; default = 15000; description = "Envoy admin, loopback."; };
      httpPort = lib.mkOption { type = lib.types.port; default = 15001; description = "h2c listener, loopback."; };
      zenohPort = lib.mkOption { type = lib.types.port; default = 17447; description = "Zenoh TCP listener, loopback."; };
      package = lib.mkPackageOption pkgs "envoy" { };
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.robot = {
      isNormalUser = true;
      inherit (cfg) uid;
      group = "robot";
    };
    users.groups.robot = { };
    users.users.mw-envoy = { isSystemUser = true; group = "mw-envoy"; };
    users.groups.mw-envoy = { };

    systemd.tmpfiles.rules = [
      "d ${stateDir} 0755 root root -"
      "d ${spiffeDir} 0750 robot robot -"
    ];

    environment.systemPackages = [ pkgs.spiffe-helper cfg.envoy.package ];
    environment.variables.SPIFFE_ENDPOINT_SOCKET = agentSocket;

    services.spire.agent = {
      enable = true;
      settings = {
        agent = {
          trust_domain = cfg.trustDomain;
          server_address = "$MW_SPIRE_SERVER_ADDRESS";
          server_port = cfg.spireServerPort;
          join_token_file = "$CREDENTIALS_DIRECTORY/join-token";
          trust_bundle_path = "$CREDENTIALS_DIRECTORY/bundle.spiffe";
          trust_bundle_format = "spiffe";
        };
        plugins = {
          KeyManager.disk.plugin_data.directory = "$STATE_DIRECTORY";
          NodeAttestor.join_token.plugin_data = { };
          WorkloadAttestor.unix.plugin_data = { };
        };
      };
    };
    systemd.services.spire-agent = {
      unitConfig.ConditionPathExists = [ "${stateDir}/join-token" "${stateDir}/bundle.spiffe" ];
      environment = { inherit (cfg.environmentDefaults) MW_SPIRE_SERVER_ADDRESS; };
      serviceConfig = {
        EnvironmentFile = "-${envFile}";
        LoadCredential = [
          "join-token:${stateDir}/join-token"
          "bundle.spiffe:${stateDir}/bundle.spiffe"
        ];
        RestartSec = "5s";
      };
    };

    systemd.services.spiffe-helper = {
      description = "spiffe-helper: the robot's X509-SVID on disk";
      wantedBy = [ "multi-user.target" ];
      after = [ "spire-agent.service" ];
      serviceConfig = {
        ExecStart = "${pkgs.spiffe-helper}/bin/spiffe-helper -config ${helperConf}";
        User = "robot";
        Group = "robot";
        Restart = "always";
        RestartSec = "5s";
      };
    };

    # A new bundle.pem is the last file spiffe-helper writes per renewal.
    systemd.paths.mw-envoy-certs = {
      wantedBy = [ "multi-user.target" ];
      pathConfig.PathChanged = "${spiffeDir}/bundle.pem";
    };
    systemd.services.mw-envoy-certs = {
      description = "Copy the robot SVID to Envoy";
      after = [ "mw-envoy.service" ];
      serviceConfig = { Type = "oneshot"; ExecStart = copyCerts; };
    };

    systemd.services.mw-envoy = {
      description = "Envoy: robot loopback listeners, mTLS to the mesh gateway";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      environment = cfg.environmentDefaults;
      serviceConfig = {
        EnvironmentFile = "-${envFile}";
        User = "mw-envoy";
        Group = "mw-envoy";
        RuntimeDirectory = "mw-envoy";
        RuntimeDirectoryMode = "0750";
        ExecStartPre = envoyPrepare;
        ExecStart = "${cfg.envoy.package}/bin/envoy --log-level info -c /run/mw-envoy/envoy.yaml";
        # Pick up an SVID issued before Envoy started.
        ExecStartPost = "+${pkgs.bash}/bin/sh -c '[ -s ${spiffeDir}/bundle.pem ] && ${copyCerts} || true'";
        Restart = "always";
        RestartSec = "2s";
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
      };
    };

    systemd.services.mw-robot = {
      description = "Robot app (h2c and Zenoh through the local Envoy)";
      wantedBy = [ "multi-user.target" ];
      after = [ "mw-envoy.service" "spiffe-helper.service" ];
      environment = cfg.environmentDefaults // {
        MW_ROBOT_NAME = cfg.name;
        MW_ROBOT_UPSTREAM = "http://127.0.0.1:${toString cfg.envoy.httpPort}";
        MW_ZENOH_ROUTER = "tcp/127.0.0.1:${toString cfg.envoy.zenohPort}";
        MW_SPIFFE_DIR = spiffeDir;
        SPIFFE_ENDPOINT_SOCKET = agentSocket;
      };
      serviceConfig = {
        EnvironmentFile = "-${envFile}";
        ExecStart = lib.escapeShellArgs ([ (lib.getExe cfg.package) ] ++ cfg.args);
        User = "robot";
        Group = "robot";
        Restart = "on-failure";
        RestartSec = "5s";
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
      };
    };
  };
}
