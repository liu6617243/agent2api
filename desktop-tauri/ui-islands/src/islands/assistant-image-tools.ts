import { t } from '../i18n'
import { callJson } from './assistant-state'
import { assistantImageMeta, createAssistantImageTools } from '../../../ui-shared/assistant-image-tools'

export const ASSISTANT_IMAGE_TOOLS = createAssistantImageTools(
  (path, body, signal) => callJson('POST', path, body, signal), t,
)
export const ASSISTANT_IMAGE_META = assistantImageMeta(t)
