export interface HistoryEntry {
  id: number;
  text: string;
  model: string;
  timestamp: number;
  duration_ms: number | null;
  audio_path: string | null;
  /** Recording start time in epoch ms; timestamp is transcription completion */
  started_at: number | null;
}

export interface StorageStats {
  count: number;
  audio_bytes: number;
}

export interface AppSettings {
  provider: string;
  api_key: string;
  gemini_api_key: string;
  custom_api_key: string;
  custom_base_url: string;
  tingwu_access_key_id: string;
  tingwu_access_key_secret: string;
  tingwu_app_key: string;
  tingwu_region: string;
  tingwu_oss_endpoint: string;
  tingwu_oss_bucket: string;
  tingwu_oss_prefix: string;
  model: string;
  language: string;
  proxy_mode: string;
  proxy_url: string;
  shortcut: string;
  sound_enabled: boolean;
  mic_min_gain: number;
  overlay_rx: number | null;
  overlay_ry: number | null;
}
