// Files imported `with { type: "file" }` resolve to the URL the bundler gives them.
declare module "*.woff2" {
  const url: string;
  export default url;
}
