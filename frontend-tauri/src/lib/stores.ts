import { create } from 'zustand'
import type {
  AuthState,
  UserInfo,
  BrowseResponse,
  SearchResponse,
  TrackHandle,
  PlaybackState,
  QueueItem,
  QueueMode,
  RepeatMode,
  MixType,
  StationType,
  RecommendationSet,
  Settings,
  DiagnosticEvent,
} from './tauri'

// Auth Store
interface AuthStoreState {
  state: AuthState['type'];
  user: UserInfo | null;
  loading: boolean;
  setState: (state: AuthState['type']) => void;
  setUser: (user: UserInfo | null) => void;
  setLoading: (loading: boolean) => void;
  signOut: () => void;
}

export const useAuthStore = create<AuthStoreState>()(
    (set) => ({
      state: 'unauthenticated',
      user: null,
      loading: true,
      setState: (state) => set({ state }),
      setUser: (user) => set({ user }),
      setLoading: (loading) => set({ loading }),
      signOut: () => set({ state: 'unauthenticated', user: null }),
    })
)

// Catalogue Store
interface CatalogueStoreState {
  currentBrowse: BrowseResponse | null;
  searchResults: SearchResponse | null;
  searchLoading: boolean;
  browseLoading: boolean;
  setCurrentBrowse: (browse: BrowseResponse | null) => void;
  setSearchResults: (results: SearchResponse | null) => void;
  setSearchLoading: (loading: boolean) => void;
  setBrowseLoading: (loading: boolean) => void;
}

export const useCatalogueStore = create<CatalogueStoreState>((set) => ({
  currentBrowse: null,
  searchResults: null,
  searchLoading: false,
  browseLoading: false,
  setCurrentBrowse: (browse) => set({ currentBrowse: browse }),
  setSearchResults: (results) => set({ searchResults: results }),
  setSearchLoading: (loading) => set({ searchLoading: loading }),
  setBrowseLoading: (loading) => set({ browseLoading: loading }),
}))

// Audio Store
interface AudioStoreState {
  state: PlaybackState;
  currentTrack: TrackHandle | null;
  position: number;
  duration: number;
  volume: number;
  error: string | null;
  setState: (state: PlaybackState) => void;
  setCurrentTrack: (track: TrackHandle | null) => void;
  setPosition: (position: number) => void;
  setDuration: (duration: number) => void;
  setVolume: (volume: number) => void;
  setError: (error: string | null) => void;
}

export const useAudioStore = create<AudioStoreState>((set) => ({
  state: 'stopped',
  currentTrack: null,
  position: 0,
  duration: 0,
  volume: 1,
  error: null,
  setState: (state) => set({ state }),
  setCurrentTrack: (track) => set({ currentTrack: track }),
  setPosition: (position) => set({ position }),
  setDuration: (duration) => set({ duration }),
  setVolume: (volume) => set({ volume }),
  setError: (error) => set({ error }),
}))

// Queue Store
interface QueueStoreState {
  queue: QueueItem[];
  history: QueueItem[];
  currentIndex: number;
  mode: QueueMode;
  repeatMode: RepeatMode;
  shuffle: boolean;
  setQueue: (queue: QueueItem[]) => void;
  setHistory: (history: QueueItem[]) => void;
  setCurrentIndex: (index: number) => void;
  setMode: (mode: QueueMode) => void;
  setRepeatMode: (mode: RepeatMode) => void;
  setShuffle: (shuffle: boolean) => void;
  addToQueue: (item: QueueItem) => void;
  removeFromQueue: (index: number) => void;
  moveInQueue: (from: number, to: number) => void;
  clearQueue: () => void;
}

export const useQueueStore = create<QueueStoreState>((set) => ({
  queue: [],
  history: [],
  currentIndex: 0,
  mode: 'normal',
  repeatMode: 'off',
  shuffle: false,
  setQueue: (queue) => set({ queue }),
  setHistory: (history) => set({ history }),
  setCurrentIndex: (index) => set({ currentIndex: index }),
  setMode: (mode) => set({ mode }),
  setRepeatMode: (mode) => set({ repeatMode: mode }),
  setShuffle: (shuffle) => set({ shuffle }),
  addToQueue: (item) => set((state) => ({ queue: [...state.queue, item] })),
  removeFromQueue: (index) => set((state) => ({
    queue: state.queue.filter((_, i) => i !== index),
  })),
  moveInQueue: (from, to) => set((state) => {
    const queue = [...state.queue];
    const [item] = queue.splice(from, 1);
    queue.splice(to, 0, item);
    return { queue };
  }),
  clearQueue: () => set({ queue: [], currentIndex: 0 }),
}))

// Recommendations Store
interface RecommendationsStoreState {
  currentSet: RecommendationSet | null;
  mixes: Record<MixType, RecommendationSet | null>;
  stations: Record<string, RecommendationSet | null>;
  loading: boolean;
  setCurrentSet: (set: RecommendationSet | null) => void;
  setMix: (type: MixType, set: RecommendationSet | null) => void;
  setStation: (type: StationType, set: RecommendationSet | null) => void;
  setLoading: (loading: boolean) => void;
}

export const useRecommendationsStore = create<RecommendationsStoreState>((set) => ({
  currentSet: null,
  mixes: {
    your_mix: null,
    discover_mix: null,
    new_release_mix: null,
    liked_songs_mix: null,
    supermix: null,
  },
  stations: {},
  loading: false,
  setCurrentSet: (value) => set({ currentSet: value }),
  setMix: (type, value) => set((state) => ({ mixes: { ...state.mixes, [type]: value } })),
  setStation: (type, value) => set((state) => ({ stations: { ...state.stations, [typeof type === 'string' ? type : `genre:${type.genre}`]: value } })),
  setLoading: (loading) => set({ loading }),
}))

// Settings Store
interface SettingsStoreState {
  settings: Settings | null;
  loading: boolean;
  setSettings: (settings: Settings) => void;
  setLoading: (loading: boolean) => void;
  updateSetting: <K extends keyof Settings>(key: K, value: Settings[K]) => void;
}

export const useSettingsStore = create<SettingsStoreState>()(
    (set) => ({
      settings: null,
      loading: true,
      setSettings: (settings) => set({ settings, loading: false }),
      setLoading: (loading) => set({ loading }),
      updateSetting: (key, value) => set((state) => ({
        settings: state.settings ? { ...state.settings, [key]: value } : null,
      })),
    })
)

// Diagnostics Store
interface DiagnosticsStoreState {
  events: DiagnosticEvent[];
  streamErrors: StreamErrorEvent[];
  maxEvents: number;
  addEvent: (event: DiagnosticEvent) => void;
  addStreamError: (event: StreamErrorEvent) => void;
  clearEvents: () => void;
  setMaxEvents: (max: number) => void;
}

export interface StreamErrorEvent {
  video_id: string;
  error_code: string;
  message: string;
  retryable: boolean;
  timestamp: string;
}

export const useDiagnosticsStore = create<DiagnosticsStoreState>((set) => ({
  events: [],
  streamErrors: [],
  maxEvents: 1000,
  addEvent: (event) => set((state) => ({
    events: [event, ...state.events].slice(0, state.maxEvents),
  })),
  addStreamError: (event) => set((state) => ({
    streamErrors: [event, ...state.streamErrors].slice(0, state.maxEvents),
  })),
  clearEvents: () => set({ events: [], streamErrors: [] }),
  setMaxEvents: (max) => set({ maxEvents: max }),
}))
