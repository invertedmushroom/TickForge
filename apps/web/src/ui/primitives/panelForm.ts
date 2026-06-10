import { createSectionHeader } from './formControls';

export type PanelFormSection = {
  title: string;
  hint?: string;
  rows: readonly { element: HTMLElement }[];
};

/**
 * Render an ordered list of sections into a panel body. Returned function
 * matches the `UiOverlayOptions.content` callback signature so it can be
 * passed directly to `openPanel({ content })`.
 */
export function createPanelForm(sections: readonly PanelFormSection[]): (body: HTMLElement) => void {
  return (body: HTMLElement) => {
    body.innerHTML = '';
    body.classList.add('ui-form');
    for (const section of sections) {
      body.append(createSectionHeader(section.title, section.hint));
      const group = document.createElement('div');
      group.className = 'ui-form__section';
      for (const row of section.rows) {
        group.append(row.element);
      }
      body.append(group);
    }
  };
}
