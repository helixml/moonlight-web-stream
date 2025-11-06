import CONFIG from "./config.js"

export function isCredentialAuthenticationEnabled(): boolean {
    return CONFIG?.enable_credential_authentication ?? true
}

export function buildUrl(path: string): string {
    return `${window.location.origin}${CONFIG?.path_prefix ?? ""}${path}`
}