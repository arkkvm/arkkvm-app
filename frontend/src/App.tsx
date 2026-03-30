import "./App.css";
import { Button } from "@/components/ui/button"
import { ThemeProvider } from "@/components/theme-provider"

import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog"

import { Switch } from "@/components/ui/switch"
import { LoginForm } from "@/components/login-form"

const App = () => {
  return (
    <ThemeProvider defaultTheme="dark" storageKey="ui-theme">
      <div className="flex min-h-svh w-full items-center justify-center p-6 md:p-10">
        <div className="items-center gap-2 w-full max-w-sm">
          <LoginForm />
        </div>
      </div>
      {/* <div className="static">
        <h1 className="text-3xl font-bold underline">Title</h1>
        <h2>Hello world !</h2>
      </div>
      <div className="flex items-center">
        <Button>Hello world !</Button>
        <ModeToggle />
        <Switch />
      </div>
      <AlertDialog>
        <AlertDialogTrigger><Button variant="outline">Open</Button></AlertDialogTrigger>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Are you absolutely sure?</AlertDialogTitle>
            <AlertDialogDescription>
              This action cannot be undone. This will permanently delete your account
              and remove your data from our servers.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction>Continue</AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog> */}
    </ThemeProvider>
  );
};

export default App;
