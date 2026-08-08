// Toast system: bottom-right, mono, success auto-dismisses, errors stay
// until clicked and say what to do (style guide "States" + "Copy").

import {
  createContext,
  useCallback,
  useContext,
  useMemo,
  useState,
  type ReactNode,
} from "react";

interface Toast {
  id: number;
  message: string;
  kind: "info" | "error";
}

interface ToastApi {
  toast: (message: string) => void;
  toastError: (message: string) => void;
}

const ToastContext = createContext<ToastApi>({
  toast: () => {},
  toastError: () => {},
});

export const useToasts = () => useContext(ToastContext);

let nextId = 1;

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);

  const remove = useCallback((id: number) => {
    setToasts((current) => current.filter((t) => t.id !== id));
  }, []);

  const push = useCallback(
    (message: string, kind: Toast["kind"]) => {
      const id = nextId++;
      setToasts((current) => [...current, { id, message, kind }]);
      if (kind === "info") setTimeout(() => remove(id), 4000);
    },
    [remove],
  );

  const value = useMemo<ToastApi>(
    () => ({
      toast: (message) => push(message, "info"),
      toastError: (message) => push(message, "error"),
    }),
    [push],
  );

  return (
    <ToastContext.Provider value={value}>
      {children}
      <div className="toasts" role="status" aria-live="polite">
        {toasts.map((t) => (
          <button
            key={t.id}
            className={`toast ${t.kind === "error" ? "error" : ""}`}
            onClick={() => remove(t.id)}
            style={{ textAlign: "left", cursor: "pointer" }}
          >
            {t.message}
          </button>
        ))}
      </div>
    </ToastContext.Provider>
  );
}
