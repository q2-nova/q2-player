"use strict";
const token = location.hash.slice(1);
const form = document.querySelector("form");
const status = document.querySelector("[role=status]");
if (location.hash) {
  history.replaceState(null, "", location.pathname);
}
if (/^[0-9a-f]{64}$/.test(token)) {
  form.hidden = false;
  status.textContent = "Keep your phone connected to the Q2 Wi-Fi until you send.";
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const button = form.querySelector("button");
    button.disabled = true;
    try {
      const response = await fetch("/entry", {
        method: "POST",
        credentials: "omit",
        redirect: "error",
        headers: { "Authorization": "Bearer " + token, "Content-Type": "application/json" },
        body: JSON.stringify({
          username: form.elements.username.value,
          password: form.elements.password.value,
        }),
      });
      if (!response.ok) throw new Error("entry rejected");
      form.reset();
      form.hidden = true;
      status.textContent = "Details sent. Return to your Q2.";
    } catch {
      form.elements.password.value = "";
      status.textContent = "Couldn’t send. Check the Q2 and scan its code again if pairing ended.";
      button.disabled = false;
    }
  });
}
