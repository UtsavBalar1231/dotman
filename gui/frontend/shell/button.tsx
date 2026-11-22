import { Button as ButtonPrimitive } from "@base-ui/react/button";
import type { ComponentProps } from "react";

export function Button({ className = "", ...props }: ComponentProps<typeof ButtonPrimitive>) {
  return <ButtonPrimitive data-slot="button" className={`inline-flex items-center justify-center ${className}`} {...props} />;
}
