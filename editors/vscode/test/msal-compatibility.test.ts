import { createRequire } from 'node:module';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { AuthorizationCodeCredential, EnvironmentCredential, ManagedIdentityCredential } from '@azure/identity';
import { createHttpHeaders } from '@azure/core-rest-pipeline';
import type { HttpClient } from '@azure/core-rest-pipeline';
import type { INetworkModule, NetworkRequestOptions } from '@azure/msal-node';

// Resolve exactly as the packaging tool's Identity client does, including a
// nested dependency if a future lockfile stops deduplicating MSAL.
const require = createRequire(import.meta.url);
const identityRequire = createRequire(require.resolve('@azure/identity'));
const msal = identityRequire('@azure/msal-node') as typeof import('@azure/msal-node');
const tenant = '11111111-1111-1111-1111-111111111111';
const client = '22222222-2222-2222-2222-222222222222';
const origin = 'https://login.microsoftonline.com';
const scope = '499b84ac-1321-427f-aa17-267ca6975798/.default';
const tokenResponse = {
  access_token: 'offline-compatibility-token', expires_in: 3600,
  ext_expires_in: 3600, token_type: 'Bearer', scope,
};

function mockTransport() {
  const requests: Array<{ url: URL; body: URLSearchParams }> = [];
  const httpClient: HttpClient = {
    async sendRequest(request) {
      const url = new URL(request.url);
      expect(url.origin).toBe(origin);
      expect(url.pathname).toBe(`/${tenant}/oauth2/v2.0/token`);
      expect(request.method).toBe('POST');
      expect(typeof request.body).toBe('string');
      requests.push({ url, body: new URLSearchParams(request.body as string) });
      return { request, status: 200, headers: createHttpHeaders(), bodyAsText: JSON.stringify(tokenResponse) };
    },
  };
  return { requests, httpClient };
}

afterEach(() => { vi.unstubAllEnvs(); });

describe('reviewed Azure Identity MSAL security override', () => {
  it('resolves the reviewed parent and patched MSAL pair', () => {
    expect(require('@azure/identity/package.json').version).toBe('4.13.3');
    const dependency = identityRequire('@azure/msal-node/package.json');
    expect(dependency.version).toBe('7.1.0');
    expect(dependency.engines.node).toBe('>=20');
  });

  it('supports the packaging EnvironmentCredential client-secret flow and MSAL cache reuse', async () => {
    vi.stubEnv('AZURE_TENANT_ID', tenant);
    vi.stubEnv('AZURE_CLIENT_ID', client);
    vi.stubEnv('AZURE_CLIENT_SECRET', 'offline-secret');
    vi.stubEnv('AZURE_REGIONAL_AUTHORITY_NAME', '');
    vi.stubEnv('AZURE_AUTHORITY_HOST', origin);
    const transport = mockTransport();
    const credential = new EnvironmentCredential({ httpClient: transport.httpClient, retryOptions: { maxRetries: 0 } });
    const token = await credential.getToken(scope);
    expect(token?.token).toBe(tokenResponse.access_token);
    expect(token?.expiresOnTimestamp).toBeGreaterThan(Date.now());
    expect(transport.requests).toHaveLength(1);
    expect(transport.requests[0].body.get('grant_type')).toBe('client_credentials');
    expect(transport.requests[0].body.get('client_id')).toBe(client);
    expect(transport.requests[0].body.get('client_secret')).toBe('offline-secret');
    expect((await credential.getToken(scope))?.token).toBe(tokenResponse.access_token);
    expect(transport.requests).toHaveLength(1);
  });

  it('supports Identity authorization-code requests through the patched confidential client', async () => {
    vi.stubEnv('AZURE_AUTHORITY_HOST', origin);
    const transport = mockTransport();
    const credential = new AuthorizationCodeCredential(tenant, client, 'offline-secret', 'offline-code', 'http://localhost/callback', {
      httpClient: transport.httpClient, retryOptions: { maxRetries: 0 },
    });
    expect((await credential.getToken(scope)).token).toBe(tokenResponse.access_token);
    expect(transport.requests).toHaveLength(1);
    expect(transport.requests[0].body.get('grant_type')).toBe('authorization_code');
    expect(transport.requests[0].body.get('code')).toBe('offline-code');
  });

  it('supports the packaging managed-identity alternative using an injected IMDS transport', async () => {
    for (const key of ['IDENTITY_ENDPOINT', 'IDENTITY_HEADER', 'IDENTITY_SERVER_THUMBPRINT', 'MSI_ENDPOINT', 'MSI_SECRET', 'IMDS_ENDPOINT', 'AZURE_POD_IDENTITY_AUTHORITY_HOST', 'AZURE_CLIENT_ID', 'AZURE_TENANT_ID', 'AZURE_FEDERATED_TOKEN_FILE']) {
      vi.stubEnv(key, undefined);
    }
    const requests: URL[] = [];
    const httpClient: HttpClient = {
      async sendRequest(request) {
        const url = new URL(request.url);
        expect(url.origin).toBe('http://169.254.169.254');
        expect(url.pathname).toBe('/metadata/identity/oauth2/token');
        expect(url.searchParams.get('client_id')).toBe(client);
        expect(request.headers.get('Metadata')).toBe('true');
        expect(request.method).toBe('GET');
        requests.push(url);
        return { request, status: 200, headers: createHttpHeaders(), bodyAsText: JSON.stringify({
          access_token: 'offline-managed-token', expires_in: '3600',
          expires_on: String(Math.floor(Date.now() / 1000) + 3600), token_type: 'Bearer',
          resource: 'https://management.azure.com/', client_id: client,
        }) };
      },
    };
    const credential = new ManagedIdentityCredential({ clientId: client, httpClient, retryOptions: { maxRetries: 0 } });
    expect((await credential.getToken('https://management.azure.com/.default')).token).toBe('offline-managed-token');
    expect(requests).toHaveLength(1);
  });

  it('retains state/instance-aware refusal and prevents payload endpoint or code substitution', async () => {
    const requests: Array<{ url: URL; body: URLSearchParams }> = [];
    const networkClient: INetworkModule = {
      async sendGetRequestAsync() { throw new Error('Unexpected discovery request in offline compatibility control'); },
      async sendPostRequestAsync<T>(url: string, options?: NetworkRequestOptions) {
        const endpoint = new URL(url);
        expect(endpoint.origin).toBe(origin);
        expect(endpoint.pathname).toBe(`/${tenant}/oauth2/v2.0/token`);
        requests.push({ url: endpoint, body: new URLSearchParams(options?.body) });
        return { status: 200, headers: {}, body: tokenResponse as T };
      },
    };
    const app = new msal.ConfidentialClientApplication({
      auth: { clientId: client, clientSecret: 'offline-secret', authority: `${origin}/${tenant}` },
      system: { networkClient },
    });
    const request = { scopes: [scope], redirectUri: 'http://localhost/callback', code: 'trusted-request-code', state: 'expected' };
    await expect(app.getAuthCodeUrl({ ...request, extraQueryParameters: { instance_aware: 'true' } }))
      .rejects.toMatchObject({ errorCode: 'unsupported_cca_instance_aware' });
    await expect(app.acquireTokenByCode(request, { code: 'payload-code', state: 'wrong' }))
      .rejects.toMatchObject({ errorCode: 'state_mismatch' });
    expect(requests).toHaveLength(0);
    const token = await app.acquireTokenByCode(request, {
      code: 'payload-code', state: 'expected', cloud_instance_host_name: 'attacker.invalid',
      cloud_instance_name: 'attacker', msgraph_host: 'attacker.invalid', client_info: 'untrusted',
    });
    expect(token?.accessToken).toBe(tokenResponse.access_token);
    expect(requests).toHaveLength(1);
    expect(requests[0].body.get('code')).toBe('trusted-request-code');
    const serialized = app.getTokenCache().serialize();
    app.getTokenCache().deserialize(serialized);
    expect(JSON.parse(app.getTokenCache().serialize())).toEqual(JSON.parse(serialized));
  });
});
