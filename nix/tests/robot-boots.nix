# checks.robot-boots (design.md M44, M45): the robot module boots in the nix
# sandbox, with no host network, against a stand-in for the lab.
#
#   gateway  spire-server (join_token attestor, trust domain infra.local) and
#            an mTLS "ingress": nginx on 30443 (HTTP/2, client certificate
#            required, answers `$ssl_client_verify $host`) and on 30444 (TCP
#            stream, answers `$ssl_client_verify`). Its server certificate is
#            an X509-SVID minted by the same SPIRE server, as Istio's gateway
#            certificate is in the lab.
#   robot    nix/robot.nix exactly as the robot VM runs it.
#
# The script first checks the unprovisioned robot (Envoy and the robot app
# up, spire-agent held back by its path condition), then provisions it the
# way the README tells a user to (join token, SPIFFE bundle, robot.env in
# /var/lib/mw-robot) and follows the identity end to end: attestation,
# spiffe-helper's files, Envoy's loaded SVID, and an h2c request and a TCP
# stream through Envoy that the gateway accepts only with that SVID.
{ pkgs, robotPackage }:
let
  trustDomain = "infra.local";
  adminSocket = "/run/spire/server/private/api.sock";
  gwDir = "/var/lib/gw";
in
pkgs.testers.runNixOSTest {
  name = "robot-boots";

  nodes.gateway = { lib, ... }: {
    services.spire.server = {
      enable = true;
      settings = {
        server = {
          trust_domain = trustDomain;
          bind_port = 30081;
        };
        plugins = {
          KeyManager.memory.plugin_data = { };
          DataStore.sql.plugin_data = {
            database_type = "sqlite3";
            connection_string = "$STATE_DIRECTORY/datastore.sqlite3";
          };
          NodeAttestor.join_token.plugin_data = { };
        };
      };
    };
    networking.firewall.allowedTCPPorts = [ 30081 30443 30444 ];

    services.nginx = {
      enable = true;
      # The certificate is minted by the test script, after boot.
      validateConfigFile = false;
      virtualHosts.ingress = {
        default = true;
        onlySSL = true;
        listen = [{ addr = "0.0.0.0"; port = 30443; ssl = true; }];
        http2 = true;
        sslCertificate = "${gwDir}/svid.pem";
        sslCertificateKey = "${gwDir}/key.pem";
        extraConfig = ''
          ssl_client_certificate ${gwDir}/bundle.pem;
          ssl_verify_client on;
        '';
        locations."/".return = ''200 "$ssl_client_verify $host\n"'';
      };
      streamConfig = ''
        server {
          listen 30444 ssl;
          ssl_certificate ${gwDir}/svid.pem;
          ssl_certificate_key ${gwDir}/key.pem;
          ssl_client_certificate ${gwDir}/bundle.pem;
          ssl_verify_client on;
          return "$ssl_client_verify\n";
        }
      '';
    };
    systemd.services.nginx.wantedBy = lib.mkForce [ ];
  };

  nodes.robot = {
    imports = [ ../robot.nix ];
    services.mw-robot = {
      enable = true;
      package = robotPackage;
      name = "test";
    };
    environment.systemPackages = [ pkgs.curl pkgs.openssl ];
  };

  testScript = ''
    start_all()

    with subtest("unprovisioned robot boots: envoy and the app up, agent held back"):
        robot.wait_for_unit("multi-user.target")
        robot.wait_for_unit("mw-envoy.service")
        robot.wait_until_succeeds("curl -sf http://127.0.0.1:15000/ready")
        robot.wait_for_open_port(15001)
        robot.wait_for_open_port(17447)
        robot.succeed("systemctl cat spire-agent.service spiffe-helper.service mw-robot.service")
        t.assertIn("ConditionResult=no", robot.succeed("systemctl show -p ConditionResult spire-agent.service"))
        t.assertEqual("1000", robot.succeed("id -u robot").strip())
        t.assertIn("User=robot", robot.succeed("systemctl show -p User mw-robot.service"))
        # The robot app was started (the placeholder app may exit until the
        # runner lands; its unit restarts it).
        robot.wait_until_succeeds("journalctl -u mw-robot.service --grep 'Started' -q")

    with subtest("gateway: SPIRE server, a registration and the gateway's own SVID"):
        gateway.wait_for_unit("spire-server.service")
        gateway.wait_until_succeeds("spire-server healthcheck -socketPath ${adminSocket}", timeout=60)
        token = gateway.succeed(
            "spire-server token generate -socketPath ${adminSocket} "
            "-spiffeID spiffe://${trustDomain}/robot/test/agent"
        ).split()[1]
        gateway.succeed(
            "spire-server entry create -socketPath ${adminSocket} "
            "-parentID spiffe://${trustDomain}/robot/test/agent "
            "-spiffeID spiffe://${trustDomain}/robot/test -selector unix:uid:1000"
        )
        bundle = gateway.succeed("spire-server bundle show -socketPath ${adminSocket} -format spiffe")
        gateway.succeed(
            "mkdir -p ${gwDir} && spire-server x509 mint -socketPath ${adminSocket} "
            "-spiffeID spiffe://${trustDomain}/ns/istio-ingress/sa/infra-gateway -write ${gwDir} "
            "&& chmod 0644 ${gwDir}/*"
        )
        gateway.succeed("systemctl start nginx.service")
        gateway.wait_for_open_port(30443)
        gateway.wait_for_open_port(30444)

    with subtest("provision the robot through /var/lib/mw-robot"):
        robot.succeed(f"printf %s '{token}' > /var/lib/mw-robot/join-token")
        robot.succeed(f"cat > /var/lib/mw-robot/bundle.spiffe <<'EOF'\n{bundle}\nEOF")
        robot.succeed(
            "printf '%s\\n' MW_SPIRE_SERVER_ADDRESS=gateway MW_GATEWAY_ADDRESS=gateway "
            "MW_GATEWAY_SNI=apps.${trustDomain} MW_ZENOH_GATEWAY_PORT=30444 "
            "> /var/lib/mw-robot/robot.env"
        )
        robot.succeed("systemctl restart mw-envoy.service spire-agent.service")

    with subtest("attestation: agent healthy, SVID for unix:uid:1000 on disk"):
        robot.wait_for_unit("spire-agent.service")
        robot.wait_until_succeeds("spire-agent healthcheck -socketPath /run/spire/agent/public/api.sock", timeout=90)
        robot.wait_until_succeeds("test -s /run/spiffe/bundle.pem", timeout=90)
        san = robot.succeed("openssl x509 -in /run/spiffe/svid.pem -noout -ext subjectAltName")
        t.assertIn("URI:spiffe://${trustDomain}/robot/test", san)
        t.assertEqual("robot", robot.succeed("stat -c %U /run/spiffe/svid_key.pem").strip())

    with subtest("envoy loads the SVID (file SDS reload)"):
        robot.wait_until_succeeds(
            "curl -sf http://127.0.0.1:15000/certs | grep -q 'spiffe://${trustDomain}/robot/test'", timeout=60
        )

    with subtest("h2c -> mTLS through the gateway, :authority preserved"):
        out = robot.wait_until_succeeds(
            "curl -sf --http2-prior-knowledge -H 'Host: demo-echo.apps' http://127.0.0.1:15001/", timeout=60
        )
        t.assertEqual("SUCCESS demo-echo.apps", out.strip())

    with subtest("Zenoh TCP listener -> mTLS through the gateway"):
        out = robot.wait_until_succeeds(
            "timeout 10 bash -c 'exec 3<>/dev/tcp/127.0.0.1/17447; cat <&3'", timeout=60
        )
        t.assertEqual("SUCCESS", out.strip())
  '';
}
