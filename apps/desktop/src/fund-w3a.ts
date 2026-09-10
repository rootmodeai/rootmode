import {
  Web3Auth,
  WEB3AUTH_NETWORK,
  WALLET_CONNECTORS,
  CONNECTOR_INITIAL_AUTHENTICATION_MODE,
  CHAIN_NAMESPACES,
  authConnector,
  metaMaskConnector,
} from "@web3auth/modal";

export type StartOpts = {
  clientId: string;
  network: string;
  defaultChainId: string;
  /** Public HTTPS RPC for the Web3Auth iframe. Loopback is blocked from wallet.web3auth.io. */
  rpcTarget?: string;
};

/**
 * Signed allowlist for the desktop fund page. Every machine opens the same
 * loopback origin; this is a signature of that origin, not the client secret.
 * Sapphire Mainnet will not whitelist localhost on the dashboard.
 */
const LOOPBACK_ORIGIN_DATA: Record<string, string> = {
  "http://127.0.0.1:17331":
    "MEQCIBPkSBbm420AFHib1amxqPCLreHlmQbxoSZr4wpGpxN0AiB51054IiVWGXHvSRAp_xq1TpIwHz8N_jaJGYm9hH8VcQ",
  "http://localhost:17331":
    "MEQCIB9J5kG901xGEv4O1yq93L6_zhfhTa8Q_BimTpKSkFqqAiBrmrE_MdLCI-laeyIeD1huran0RWYNCZm3bR3UKmDukA",
};

async function start(opts: StartOpts) {
  const network =
    opts.network === "sapphire_mainnet"
      ? WEB3AUTH_NETWORK.SAPPHIRE_MAINNET
      : WEB3AUTH_NETWORK.SAPPHIRE_DEVNET;
  // No uiConfig: logos/theme count as whitelabel, which 403s on the free plan.
  // Without description/mainOption, socials render as 20px icon-only buttons
  // (the "dots"). Full-length labeled buttons are the default people expect.
  const web3auth = new Web3Auth({
    clientId: opts.clientId,
    web3AuthNetwork: network,
    defaultChainId: opts.defaultChainId,
    ...(opts.rpcTarget
      ? { chains: [chainFor(opts.defaultChainId, opts.rpcTarget)] }
      : {}),
    // Connect-and-sign (SIWE) after MetaMask is the v11 default. It hits
    // /citadel-service/v1/siww/verify, 401s on this loopback origin, then
    // disconnects. Connect-only is enough: we only need an EIP-1193 provider.
    initialAuthenticationMode: CONNECTOR_INITIAL_AUTHENTICATION_MODE.CONNECT_ONLY,
    connectors: [
      authConnector({
        connectorSettings: {
          originData: LOOPBACK_ORIGIN_DATA,
        },
      }),
      metaMaskConnector({
        dapp: { name: "rootmode", url: "https://rootmode.ai" },
        ui: { preferExtension: true, headless: false, showInstallModal: true },
      }),
    ],
    modalConfig: {
      connectors: {
        [WALLET_CONNECTORS.AUTH]: {
          label: "Social",
          showOnModal: true,
          loginMethods: {
            google: {
              name: "Google",
              showOnModal: true,
              mainOption: true,
              description: "Continue with Google",
            },
            email_passwordless: {
              name: "Email",
              showOnModal: true,
              mainOption: true,
              description: "Continue with email",
            },
            apple: {
              name: "Apple",
              showOnModal: true,
              mainOption: true,
              description: "Continue with Apple",
            },
          },
        },
        [WALLET_CONNECTORS.METAMASK]: {
          label: "MetaMask",
          showOnModal: true,
        },
      },
    },
  });
  await web3auth.init();
  skipWeb3AuthSession(web3auth);
  return web3auth;
}

function chainFor(chainId: string, rpcTarget: string) {
  const base = chainId === "0x2105";
  const sepolia = chainId === "0x14a34";
  return {
    chainNamespace: CHAIN_NAMESPACES.EIP155,
    chainId,
    rpcTarget,
    displayName: base ? "Base" : sepolia ? "Base Sepolia" : "rootmode local",
    ticker: "ETH",
    tickerName: "Ether",
    decimals: 18,
    blockExplorerUrl: base
      ? "https://basescan.org"
      : sepolia
        ? "https://sepolia.basescan.org"
        : "",
    logo: "https://images.web3auth.io/ethereum.svg",
  };
}

/** v11 still SIWE-authorizes external wallets even when CONNECT_ONLY is set. */
function skipWeb3AuthSession(web3auth: Web3Auth) {
  const mode = CONNECTOR_INITIAL_AUTHENTICATION_MODE.CONNECT_ONLY;
  const anyAuth = web3auth as Web3Auth & {
    coreOptions?: { initialAuthenticationMode?: string };
    options?: { initialAuthenticationMode?: string };
    connectors?: Array<{ authorizeOrDisconnect?: (flag?: boolean, chainId?: string) => Promise<void> }>;
  };
  if (anyAuth.coreOptions) anyAuth.coreOptions.initialAuthenticationMode = mode;
  if (anyAuth.options) anyAuth.options.initialAuthenticationMode = mode;
  for (const connector of anyAuth.connectors ?? []) {
    if (typeof connector.authorizeOrDisconnect === "function") {
      connector.authorizeOrDisconnect = async () => undefined;
    }
  }
}

declare global {
  interface Window {
    RootmodeW3A: { start: typeof start };
  }
}

window.RootmodeW3A = { start };
