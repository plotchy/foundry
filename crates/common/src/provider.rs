//! Commonly used helpers to construct `Provider`s

use crate::{runtime_client::RuntimeClient, ALCHEMY_FREE_TIER_CUPS, REQUEST_TIMEOUT};
use ethers_core::types::{Chain, U256};
use ethers_middleware::gas_oracle::{GasCategory, GasOracle, Polygon};
use ethers_providers::{is_local_endpoint, Middleware, Provider, DEFAULT_LOCAL_POLL_INTERVAL};
use eyre::WrapErr;
use reqwest::{IntoUrl, Url};
use std::{borrow::Cow, env, path::Path, time::Duration};
use url::ParseError;

/// Helper type alias for a retry provider
pub type RetryProvider = Provider<RuntimeClient>;

/// Helper type alias for a rpc url
pub type RpcUrl = String;

/// Same as `try_get_http_provider`
///
/// # Panics
///
/// If invalid URL
///
/// # Example
///
/// ```
/// use foundry_common::get_http_provider;
/// # fn f() {
///  let retry_provider = get_http_provider("http://localhost:8545");
/// # }
/// ```
pub fn get_http_provider(builder: impl Into<ProviderBuilder>) -> RetryProvider {
    try_get_http_provider(builder).unwrap()
}

/// Gives out a provider with a `100ms` interval poll if it's a localhost URL (most likely an anvil
/// or other dev node) and with the default, `7s` if otherwise.
pub fn try_get_http_provider(builder: impl Into<ProviderBuilder>) -> eyre::Result<RetryProvider> {
    builder.into().build()
}

/// Helper type to construct a `RetryProvider`
#[derive(Debug)]
pub struct ProviderBuilder {
    // Note: this is a result, so we can easily chain builder calls
    url: eyre::Result<Url>,
    chain: Chain,
    max_retry: u32,
    timeout_retry: u32,
    initial_backoff: u64,
    timeout: Duration,
    /// available CUPS
    compute_units_per_second: u64,
    /// JWT Secret
    jwt: Option<String>,
}

// === impl ProviderBuilder ===

impl ProviderBuilder {
    /// Creates a new builder instance
    pub fn new(url: impl IntoUrl) -> Self {
        let url_str = url.as_str();
        if url_str.starts_with("localhost:") {
            // invalid url: non-prefixed URL scheme is not allowed, so we prepend the default http
            // prefix
            return Self::new(format!("http://{url_str}"))
        }

        let url = Url::parse(url_str)
            .or_else(|err| {
                match err {
                    ParseError::RelativeUrlWithoutBase => {
                        let path = Path::new(url_str);
                        let absolute_path = if path.is_absolute() {
                            path.to_path_buf()
                        } else {
                            // Assume the path is relative to the current directory.
                            // Don't use `std::fs::canonicalize` as it requires the path to exist.
                            // It should be possible to construct a provider and only
                            // attempt to establish a connection later
                            let current_dir =
                                env::current_dir().expect("Current directory should exist");
                            current_dir.join(path)
                        };

                        let path_str =
                            absolute_path.to_str().expect("Path should be a valid string");

                        // invalid url: non-prefixed URL scheme is not allowed, so we assume the URL
                        // is for a local file
                        Url::parse(format!("file://{path_str}").as_str())
                    }
                    _ => Err(err),
                }
            })
            .wrap_err(format!("Invalid provider url: {url_str}"));

        Self {
            url,
            chain: Chain::Mainnet,
            max_retry: 100,
            timeout_retry: 5,
            initial_backoff: 100,
            timeout: REQUEST_TIMEOUT,
            // alchemy max cpus <https://github.com/alchemyplatform/alchemy-docs/blob/master/documentation/compute-units.md#rate-limits-cups>
            compute_units_per_second: ALCHEMY_FREE_TIER_CUPS,
            jwt: None,
        }
    }

    /// Enables a request timeout.
    ///
    /// The timeout is applied from when the request starts connecting until the
    /// response body has finished.
    ///
    /// Default is no timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the chain of the node the provider will connect to
    pub fn chain(mut self, chain: impl Into<foundry_config::Chain>) -> Self {
        if let foundry_config::Chain::Named(chain) = chain.into() {
            self.chain = chain;
        }
        self
    }

    /// How often to retry a failed request
    pub fn max_retry(mut self, max_retry: u32) -> Self {
        self.max_retry = max_retry;
        self
    }

    /// How often to retry a failed request due to connection issues
    pub fn timeout_retry(mut self, timeout_retry: u32) -> Self {
        self.timeout_retry = timeout_retry;
        self
    }

    /// The starting backoff delay to use after the first failed request
    pub fn initial_backoff(mut self, initial_backoff: u64) -> Self {
        self.initial_backoff = initial_backoff;
        self
    }

    /// Sets the number of assumed available compute units per second
    ///
    /// See also, <https://github.com/alchemyplatform/alchemy-docs/blob/master/documentation/compute-units.md#rate-limits-cups>
    pub fn compute_units_per_second(mut self, compute_units_per_second: u64) -> Self {
        self.compute_units_per_second = compute_units_per_second;
        self
    }

    /// Sets the number of assumed available compute units per second
    ///
    /// See also, <https://github.com/alchemyplatform/alchemy-docs/blob/master/documentation/compute-units.md#rate-limits-cups>
    pub fn compute_units_per_second_opt(mut self, compute_units_per_second: Option<u64>) -> Self {
        if let Some(cups) = compute_units_per_second {
            self.compute_units_per_second = cups;
        }
        self
    }

    /// Sets aggressive `max_retry` and `initial_backoff` values
    ///
    /// This is only recommend for local dev nodes
    pub fn aggressive(self) -> Self {
        self.max_retry(100).initial_backoff(100)
    }

    /// Sets the JWT secret
    pub fn jwt(mut self, jwt: impl Into<String>) -> Self {
        self.jwt = Some(jwt.into());
        self
    }

    /// Same as [`Self:build()`] but also retrieves the `chainId` in order to derive an appropriate
    /// interval
    pub async fn connect(self) -> eyre::Result<RetryProvider> {
        let mut provider = self.build()?;
        if let Some(blocktime) = provider.get_chainid().await.ok().and_then(|id| {
            Chain::try_from(id).ok().and_then(|chain| chain.average_blocktime_hint())
        }) {
            provider = provider.interval(blocktime / 2);
        }
        Ok(provider)
    }

    /// Constructs the `RetryProvider` taking all configs into account
    pub fn build(self) -> eyre::Result<RetryProvider> {
        let ProviderBuilder {
            url,
            chain,
            max_retry,
            timeout_retry,
            initial_backoff,
            timeout,
            compute_units_per_second,
            jwt,
        } = self;
        let url = url?;

        let mut provider = Provider::new(RuntimeClient::new(
            url.clone(),
            max_retry,
            timeout_retry,
            initial_backoff,
            timeout,
            compute_units_per_second,
            jwt,
        ));

        let is_local = is_local_endpoint(url.as_str());

        if is_local {
            provider = provider.interval(DEFAULT_LOCAL_POLL_INTERVAL);
        } else if let Some(blocktime) = chain.average_blocktime_hint() {
            provider = provider.interval(blocktime / 2);
        }
        Ok(provider)
    }
}

impl<'a> From<&'a str> for ProviderBuilder {
    fn from(url: &'a str) -> Self {
        Self::new(url)
    }
}

impl<'a> From<&'a String> for ProviderBuilder {
    fn from(url: &'a String) -> Self {
        url.as_str().into()
    }
}

impl From<String> for ProviderBuilder {
    fn from(url: String) -> Self {
        url.as_str().into()
    }
}

impl<'a> From<Cow<'a, str>> for ProviderBuilder {
    fn from(url: Cow<'a, str>) -> Self {
        url.as_ref().into()
    }
}

/// Estimates EIP1559 fees depending on the chain
///
/// Uses custom gas oracles for
///   - polygon
///
/// Fallback is the default [`Provider::estimate_eip1559_fees`] implementation
pub async fn estimate_eip1559_fees<M: Middleware>(
    provider: &M,
    chain: Option<u64>,
) -> eyre::Result<(U256, U256)>
where
    M::Error: 'static,
{
    let chain = if let Some(chain) = chain {
        chain
    } else {
        provider.get_chainid().await.wrap_err("Failed to get chain id")?.as_u64()
    };

    if let Ok(chain) = Chain::try_from(chain) {
        // handle chains that deviate from `eth_feeHistory` and have their own oracle
        match chain {
            Chain::Polygon | Chain::PolygonMumbai => {
                let estimator = Polygon::new(chain)?.category(GasCategory::Standard);
                return Ok(estimator.estimate_eip1559_fees().await?)
            }
            _ => {}
        }
    }
    provider.estimate_eip1559_fees(None).await.wrap_err("Failed fetch EIP1559 fees")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_auto_correct_missing_prefix() {
        let builder = ProviderBuilder::new("localhost:8545");
        assert!(builder.url.is_ok());

        let url = builder.url.unwrap();
        assert_eq!(url, Url::parse("http://localhost:8545").unwrap());
    }
}
