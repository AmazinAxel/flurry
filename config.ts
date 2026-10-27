import settings from './settings';

const { sideview = {} }: { sideview?: Record<string, string> } = settings;

export default {
  sideview: {
    assistant: 'https://claude.ai/new/',
    custom: 'https://beta-docs.skriptlang.org/syntaxes/',
    ...sideview,
  } as Record<string, string>,
};
