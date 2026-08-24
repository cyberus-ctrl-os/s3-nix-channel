{ self, system }:
let
  pkgs = self.inputs.nixpkgs.legacyPackages.${system};

  accessKey = "GKaaaaaaaaaaaaaaaaaaaaaaaa";
  secretKey = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
  region = "eu-central-1";
  bucket = "channels";

  secretsFile = pkgs.writeText "fake-secrets" ''
    AWS_ACCESS_KEY_ID=${accessKey}
    AWS_SECRET_ACCESS_KEY=${secretKey}
    AWS_REGION=${region}
    AWS_ENDPOINT_URL=http://s3:9000
  '';

  tarball =
    pkgs.runCommand "tarball-1234.tar.xz"
      {
        nativeBuildInputs = [ pkgs.libarchive ];
      }
      ''
        mkdir $out

        mkdir foo
        touch foo/hello

        # The original tarball.
        tar -cJf $out/tarball-1234.tar.xz foo

        # Create an updated tarball.
        touch foo/world
        tar -cJf $out/tarball-1235.tar.xz foo

        # Create yet another updated tarball.
        touch foo/again
        tar -cJf $out/tarball-1236.tar.xz foo
      '';

  isoImage =
    pkgs.runCommand "media-1234.iso"
      {
        nativeBuildInputs = [ pkgs.cdrkit ];
      }
      ''
        mkdir root
        mkdir $out

        genisoimage -o $out/media-1234.iso root/

        touch root/updated
        genisoimage -o $out/media-1235.iso root/
      '';

  tarballServeCommon =
    { config, pkgs, ... }:
    {
      nix.extraOptions = ''
        experimental-features = nix-command flakes
      '';

      environment.systemPackages = with pkgs; [
        # For tarball uploads.
        config.services.s3-nix-channel.package

        git
        jq
      ];

      services.s3-nix-channel = {
        enable = true;
        secretsFile = "${secretsFile}";
        listen = "0.0.0.0:80";
        baseUrl = "http://localhost";

        inherit bucket;
      };
    };

  rsaKeypair =
    pkgs.runCommand "rsa-keypair"
      {
        nativeBuildInputs = [
          pkgs.openssl
          pkgs.openssh
          pkgs.jwt-cli
        ];
      }
      ''
        mkdir -p $out
        ssh-keygen -t rsa -b 4096 -E SHA256 -m PEM -P "" -f $out/private.pem
        openssl rsa -pubout -in $out/private.pem -out $out/public.pem

        jwt encode --alg RS256 --exp=100y -S @$out/private.pem > $out/jwt
      '';
in
{
  canServeFiles = pkgs.testers.nixosTest {
    name = "s3-nix-channel";

    nodes = {
      s3 = {
        services.garage = {
          enable = true;
          package = pkgs.garage_2;

          settings = {
            replication_factor = 1;
            consistent_mode = "consistent";

            rpc_bind_addr = "[::]:3901";
            rpc_public_addr = "[::]:3901";
            rpc_secret = "5c1915fa04d0b6739675c61bf5907eb0fe3d9c69850c83820f51b4d25d13868c";

            s3_api = {
              s3_region = region;
              api_bind_addr = "[::]:9000";
              root_domain = ".s3.garage";
            };

          };
        };

        environment.systemPackages = [
          pkgs.minio-client
        ];

        networking.firewall.enable = false;
      };

      servePublic = {
        imports = [
          self.nixosModules.default
          tarballServeCommon
        ];
      };

      servePrivate = {
        imports = [
          self.nixosModules.default
          tarballServeCommon
        ];

        services.s3-nix-channel = {
          jwtPublicKey = "${rsaKeypair}/public.pem";
        };
      };

    };

    testScript = ''
      s3.start()
      s3.wait_for_unit("garage.service")
      s3.wait_for_open_port(3901)

      # Create cluster
      node_id = s3.succeed("garage status | tail -n1 | cut -d' ' -f1")
      s3.succeed(f"garage layout assign -z dc1 -c 128M {node_id}")
      s3.succeed("garage layout apply --version 1")

      # Create bucket
      s3.succeed("garage bucket create ${bucket}")

      # Create access keys
      s3.succeed("garage key import ${accessKey} ${secretKey} --yes")
      s3.succeed("garage bucket allow --read --write --owner ${bucket} --key ${accessKey}")

      s3.wait_for_open_port(9000)

      ## Prepare the bucket of tarballs with configuration.

      # Garage sometimes takes a second to come up.
      s3.wait_until_succeeds("mc alias set local http://localhost:9000 ${accessKey} ${secretKey}")

      ## Start our server that doesn't require authentication.
      servePublic.start()

      # It should come up with an empty bucket.
      servePublic.wait_for_unit("s3-nix-channel.service")

      # Populate the configuration
      servePublic.succeed("mkdir content")
      servePublic.copy_from_host("${tarball}/tarball-1234.tar.xz", "content/tarball-1234.tar.xz");
      servePublic.copy_from_host("${tarball}/tarball-1235.tar.xz", "content/tarball-1235.tar.xz");
      servePublic.copy_from_host("${tarball}/tarball-1236.tar.xz", "content/tarball-1236.tar.xz");
      servePublic.copy_from_host("${isoImage}/media-1234.iso", "content/media-1234.iso")
      servePublic.copy_from_host("${isoImage}/media-1235.iso", "content/media-1235.iso")

      servePublic.succeed("env $(cat ${secretsFile}) s3-nix-channel-upload add-channel ${bucket} thechannel-24.05 .tar.xz")
      servePublic.succeed("env $(cat ${secretsFile}) s3-nix-channel-upload add-channel ${bucket} install-24.05 .iso")

      servePublic.succeed("env $(cat ${secretsFile}) s3-nix-channel-upload publish ${bucket} thechannel-24.05 content/tarball-1234.tar.xz")
      servePublic.succeed("env $(cat ${secretsFile}) s3-nix-channel-upload publish ${bucket} install-24.05 content/media-1234.iso")

      servePublic.succeed("systemctl restart s3-nix-channel.service")

      servePublic.succeed("curl -vL http://localhost/channel/thechannel-24.05.tar.xz > latest.tar.xz")
      servePublic.succeed("curl -vL http://localhost/permanent/tarball-1234.tar.xz > permanent.tar.xz")

      servePublic.succeed("cmp content/tarball-1234.tar.xz latest.tar.xz")
      servePublic.succeed("cmp content/tarball-1234.tar.xz permanent.tar.xz")

      # Now with HEAD requests
      assert "200" == servePublic.succeed("curl -s -o /dev/null -w '%{http_code}' --head -vL http://localhost/channel/thechannel-24.05.tar.xz")
      assert "200" == servePublic.succeed("curl -s -o /dev/null -w '%{http_code}' --head -vL http://localhost/permanent/tarball-1234.tar.xz")

      # Now the same with custom file endings
      servePublic.succeed("curl -vL http://localhost/channel/install-24.05.iso > latest.iso")
      servePublic.succeed("curl -vL http://localhost/permanent/media-1234.iso > permanent.iso")

      servePublic.succeed("cmp content/media-1234.iso latest.iso")
      servePublic.succeed("cmp content/media-1234.iso permanent.iso")

      # Check whether we don't accidentally serve the config files
      servePublic.fail("curl --fail -vL http://localhost/channels.json")
      servePublic.fail("curl --fail -vL http://localhost/thechannel-24.05.json")
      servePublic.fail("curl --fail -vL http://localhost/install-24.05.json")

      # Add an alias
      servePublic.succeed("env $(cat ${secretsFile}) s3-nix-channel-upload add-alias ${bucket} thechannel-24.05 foobar-24.05")

      # Fail to add duplicated aliases.
      servePublic.fail("env $(cat ${secretsFile}) s3-nix-channel-upload add-alias ${bucket} thechannel-24.05 foobar-24.05")

      # Fail to add other channel names as aliases.
      servePublic.fail("env $(cat ${secretsFile}) s3-nix-channel-upload add-alias ${bucket} thechannel-24.05 install-24.05")

      # Can fetch via alias
      servePublic.succeed("systemctl restart s3-nix-channel.service")
      servePublic.succeed("curl -vL http://localhost/channel/foobar-24.05.tar.xz > alias.tar.xz")
      servePublic.succeed("cmp content/tarball-1234.tar.xz alias.tar.xz")

      # Can publish via alias
      servePublic.succeed("env $(cat ${secretsFile}) s3-nix-channel-upload publish ${bucket} foobar-24.05 content/tarball-1235.tar.xz")
      servePublic.succeed("systemctl restart s3-nix-channel.service")
      servePublic.succeed("curl -vL http://localhost/channel/foobar-24.05.tar.xz > alias-new.tar.xz")
      servePublic.succeed("cmp content/tarball-1235.tar.xz alias-new.tar.xz")
      servePublic.succeed("curl -vL http://localhost/channel/thechannel-24.05.tar.xz > alias-new2.tar.xz")
      servePublic.succeed("cmp content/tarball-1235.tar.xz alias-new2.tar.xz")

      ## Start our server that requires authentication
      servePrivate.start()
      servePrivate.wait_for_unit("s3-nix-channel.service")

      # Unauthorized requests are rejected.
      assert "401" == servePrivate.succeed("curl -s -o /dev/null -w '%{http_code}' http://localhost/channel/thechannel-24.05.tar.xz")
      assert "401" == servePrivate.succeed("curl -s -o /dev/null -w '%{http_code}' http://localhost/permanent/tarball-1234.tar.xz")

      # Authorized accesses succeed.
      servePrivate.copy_from_host("${rsaKeypair}/jwt", "jwt")
      assert "200" == servePrivate.succeed("curl -Ls -u :$(cat jwt) --basic -o /dev/null -w \'%{http_code}\' http://localhost/channel/thechannel-24.05.tar.xz")
      assert "200" == servePrivate.succeed("curl -Ls -u :$(cat jwt) --basic -o /dev/null -w \'%{http_code}\' http://localhost/permanent/tarball-1234.tar.xz")

      # Also HEAD requests
      assert "200" == servePrivate.succeed("curl --head -Ls -u :$(cat jwt) --basic -o /dev/null -w \'%{http_code}\' http://localhost/channel/thechannel-24.05.tar.xz")
      assert "200" == servePrivate.succeed("curl --head -Ls -u :$(cat jwt) --basic -o /dev/null -w \'%{http_code}\' http://localhost/permanent/tarball-1234.tar.xz")

      ## Check whether the channel works as flake input.
      servePrivate.succeed("mkdir -p flake ~/.config/nix")
      servePrivate.succeed("echo netrc-file = $HOME/.netrc > ~/.config/nix/nix.conf")
      servePrivate.succeed("echo machine localhost password $(cat jwt) > ~/.netrc")
      servePrivate.copy_from_host("${./test-flake.nix}", "flake/flake.nix")
      servePrivate.succeed("cd flake ; git init ; git add flake.nix ; nix flake lock")

      # Check whether the lock file records the right permanent URL.
      assert "http://localhost/permanent/tarball-1235.tar.xz\n" == servePrivate.succeed("jq -r .nodes.thechannel.locked.url flake/flake.lock")

      # Check whether we can update the tarball.
      servePrivate.copy_from_host("${tarball}/tarball-1236.tar.xz", "tarball-1236.tar.xz")
      print(servePrivate.succeed("env $(cat ${secretsFile}) s3-nix-channel-upload publish ${bucket} thechannel-24.05 tarball-1236.tar.xz"))

      # Check whether we can update files with different extensions as well.
      servePrivate.copy_from_host("${isoImage}/media-1235.iso", "media-1235.iso")
      print(servePrivate.succeed("env $(cat ${secretsFile}) s3-nix-channel-upload publish ${bucket} install-24.05 media-1235.iso"))

      # Check adding a channel works
      servePrivate.succeed("env $(cat ${secretsFile}) s3-nix-channel-upload add-channel ${bucket} new-24.05 txt")

      # Fail to upload duplicate files
      servePrivate.fail("env $(cat ${secretsFile}) s3-nix-channel-upload publish ${bucket} install-24.05 media-1235.iso")

      # Fail to add bogus channel name "channels"
      servePrivate.fail("env $(cat ${secretsFile}) s3-nix-channel-upload add-channel ${bucket} channels ext")

      # Fail to add duplicate channel
      servePrivate.fail("env $(cat ${secretsFile}) s3-nix-channel-upload add-channel ${bucket} install-24.05 iso")

      # Force a reload to pick up the new version.
      servePrivate.succeed("systemctl restart s3-nix-channel.service")

      # Check whether the flake updates to the new version
      servePrivate.succeed("cd flake ; nix flake update")
      assert "http://localhost/permanent/tarball-1236.tar.xz\n" == servePrivate.succeed("jq -r .nodes.thechannel.locked.url flake/flake.lock")
    '';
  };
}
